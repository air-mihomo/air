use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use air_error::{AppResult, PlatformError};
use air_platform::privilege::ELEVATED_CORE_HELPER_ARG;
use air_telemetry::log_retention::{format_current_log_timestamp, prepare_managed_log_for_append};
use air_telemetry::redaction::redact_log_value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElevatedProcessConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub current_dir: PathBuf,
    pub console_log_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ElevatedChild {
    #[cfg(windows)]
    handle: isize,
    #[cfg(unix)]
    pid_file: Option<PathBuf>,
    pid: u32,
}

impl ElevatedChild {
    pub fn spawn(config: &ElevatedProcessConfig) -> AppResult<Self> {
        spawn_elevated_process(config)
    }

    #[cfg(target_os = "macos")]
    pub fn adopt_macos_pid(pid: u32) -> Self {
        Self {
            pid_file: None,
            pid,
        }
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn try_wait(&self) -> AppResult<Option<Option<i32>>> {
        elevated_try_wait(self)
    }

    pub fn wait_timeout(&self, timeout: Duration) -> AppResult<Option<Option<i32>>> {
        elevated_wait_timeout(self, timeout)
    }

    pub fn kill(&self) -> AppResult<()> {
        elevated_kill(self)
    }

    #[cfg(windows)]
    fn raw_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle as windows_sys::Win32::Foundation::HANDLE
    }
}

#[cfg(target_os = "macos")]
pub fn terminate_macos_process_with_admin(pid: u32, timeout: Duration) -> AppResult<()> {
    terminate_macos_pid(pid, timeout)
}

#[cfg(windows)]
impl Drop for ElevatedChild {
    fn drop(&mut self) {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.raw_handle());
        }
    }
}

#[cfg(windows)]
fn spawn_elevated_process(config: &ElevatedProcessConfig) -> AppResult<ElevatedChild> {
    use std::mem::{size_of, zeroed};

    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::GetProcessId;
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NO_CONSOLE, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let current_exe = std::env::current_exe().map_err(|error| {
        PlatformError::OperationFailed(format!("读取当前程序路径失败: {error}"))
    })?;
    let operation = wide_null("runas");
    let file = wide_os_null(current_exe.as_os_str());
    let helper_args = elevated_core_helper_args(config);
    let parameters = wide_null(&air_platform::privilege::join_windows_args(&helper_args));
    let directory = wide_os_null(config.current_dir.as_os_str());

    let mut execute_info: SHELLEXECUTEINFOW = unsafe { zeroed() };
    execute_info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    execute_info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE;
    execute_info.lpVerb = operation.as_ptr();
    execute_info.lpFile = file.as_ptr();
    execute_info.lpParameters = parameters.as_ptr();
    execute_info.lpDirectory = directory.as_ptr();
    execute_info.nShow = SW_HIDE;

    // ShellExecuteExW 无法把 stdout/stderr 交给普通权限的 GUI；这里提权启动当前程序的隐藏 helper。
    // helper 再启动 mihomo、写 core.log、等待退出，GUI 仍只持有一个可等待/终止的提权进程句柄。
    let ok = unsafe { ShellExecuteExW(&mut execute_info) };
    if ok == 0 || execute_info.hProcess.is_null() {
        return Err(PlatformError::OperationFailed(format!(
            "管理员权限申请被取消或核心启动失败，ShellExecuteExW={}",
            unsafe { GetLastError() }
        ))
        .into());
    }

    let pid = unsafe { GetProcessId(execute_info.hProcess) };
    Ok(ElevatedChild {
        handle: execute_info.hProcess as isize,
        pid,
    })
}

#[cfg(target_os = "macos")]
fn spawn_elevated_process(config: &ElevatedProcessConfig) -> AppResult<ElevatedChild> {
    let pid_file = std::env::temp_dir().join(format!("air-mihomo-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    if let Some(path) = &config.console_log_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(air_error::StorageError::Io)?;
        }
        prepare_managed_log_for_append(path).map_err(|error| {
            PlatformError::OperationFailed(format!("准备核心日志文件失败: {error}"))
        })?;
    }

    let shell_script = macos_elevated_shell_script(config, &pid_file);
    let apple_script = macos_admin_osascript(&shell_script);
    let status = run_macos_admin_osascript(&apple_script, "启动")?;
    if !status.success() {
        return Err(PlatformError::OperationFailed(format!(
            "macOS 管理员授权被取消或核心启动失败，osascript={:?}。如果系统授权弹窗没有出现，请在终端运行 scripts/verify-macos-tun.sh --sudo 验证 TUN 权限。",
            status.code()
        ))
        .into());
    }

    let pid = read_pid_file(&pid_file)?;
    Ok(ElevatedChild {
        pid_file: Some(pid_file),
        pid,
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn spawn_elevated_process(_config: &ElevatedProcessConfig) -> AppResult<ElevatedChild> {
    Err(PlatformError::Unsupported("当前平台不支持单独提权启动核心进程".into()).into())
}

#[cfg(any(windows, test))]
fn elevated_core_helper_args(config: &ElevatedProcessConfig) -> Vec<String> {
    let mut args = vec![
        ELEVATED_CORE_HELPER_ARG.to_string(),
        "--program".to_string(),
        config.program.to_string_lossy().to_string(),
        "--cwd".to_string(),
        config.current_dir.to_string_lossy().to_string(),
    ];
    if let Some(path) = &config.console_log_path {
        args.push("--log".to_string());
        args.push(path.to_string_lossy().to_string());
    }
    for (key, value) in &config.env {
        args.push("--env".to_string());
        args.push(format!("{key}={value}"));
    }
    for arg in &config.args {
        args.push("--arg".to_string());
        args.push(arg.clone());
    }
    args
}

#[derive(Debug)]
pub struct ElevatedCoreHelperConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub current_dir: PathBuf,
    pub console_log_path: Option<PathBuf>,
}

pub fn elevated_core_helper_requested() -> bool {
    std::env::args().any(|arg| arg == ELEVATED_CORE_HELPER_ARG)
}

pub fn run_elevated_core_helper_from_env() -> AppResult<()> {
    let config = parse_elevated_core_helper_args(std::env::args().skip(1))?;
    run_elevated_core_helper(config)
}

fn parse_elevated_core_helper_args(
    args: impl IntoIterator<Item = String>,
) -> AppResult<ElevatedCoreHelperConfig> {
    let mut program = None;
    let mut current_dir = None;
    let mut console_log_path = None;
    let mut env = BTreeMap::new();
    let mut core_args = Vec::new();
    let mut iter = args
        .into_iter()
        .filter(|arg| arg != ELEVATED_CORE_HELPER_ARG);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--program" => program = iter.next().map(PathBuf::from),
            "--cwd" => current_dir = iter.next().map(PathBuf::from),
            "--log" => console_log_path = iter.next().map(PathBuf::from),
            "--arg" => {
                if let Some(value) = iter.next() {
                    core_args.push(value);
                }
            }
            "--env" => {
                if let Some(value) = iter.next() {
                    let (key, value) = value.split_once('=').ok_or_else(|| {
                        PlatformError::OperationFailed(format!(
                            "核心 helper 环境变量格式无效: {value}"
                        ))
                    })?;
                    env.insert(key.to_string(), value.to_string());
                }
            }
            other => {
                return Err(PlatformError::OperationFailed(format!(
                    "未知核心 helper 参数: {other}"
                ))
                .into());
            }
        }
    }
    Ok(ElevatedCoreHelperConfig {
        program: program.ok_or_else(|| PlatformError::OperationFailed("缺少核心路径".into()))?,
        args: core_args,
        env,
        current_dir: current_dir
            .ok_or_else(|| PlatformError::OperationFailed("缺少核心工作目录".into()))?,
        console_log_path,
    })
}

fn run_elevated_core_helper(config: ElevatedCoreHelperConfig) -> AppResult<()> {
    append_helper_log(
        config.console_log_path.as_deref(),
        "stdout",
        "air elevated helper starting mihomo",
    );
    let mut command = Command::new(&config.program);
    command
        .args(&config.args)
        .current_dir(&config.current_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    air_platform::process::hide_std_subprocess_window(&mut command);
    for (key, value) in &config.env {
        command.env(key, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("提权 helper 启动 mihomo 失败: {error}");
            append_helper_log(config.console_log_path.as_deref(), "stderr", &message);
            return Err(PlatformError::OperationFailed(message).into());
        }
    };
    let _job = assign_child_to_kill_on_close_job(&child);
    let stdout_thread = child
        .stdout
        .take()
        .map(|stream| helper_log_reader(stream, config.console_log_path.clone(), "stdout"));
    let stderr_thread = child
        .stderr
        .take()
        .map(|stream| helper_log_reader(stream, config.console_log_path.clone(), "stderr"));
    let status = child.wait().map_err(|error| {
        let message = format!("等待提权 mihomo 退出失败: {error}");
        append_helper_log(config.console_log_path.as_deref(), "stderr", &message);
        PlatformError::OperationFailed(message)
    })?;
    if let Some(thread) = stdout_thread {
        let _ = thread.join();
    }
    if let Some(thread) = stderr_thread {
        let _ = thread.join();
    }
    append_helper_log(
        config.console_log_path.as_deref(),
        "stdout",
        &format!(
            "air elevated helper observed mihomo exit: {:?}",
            status.code()
        ),
    );
    std::process::exit(status.code().unwrap_or(1));
}

fn helper_log_reader<T>(
    stream: T,
    console_log_path: Option<PathBuf>,
    stream_name: &'static str,
) -> thread::JoinHandle<()>
where
    T: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            append_helper_log(
                console_log_path.as_deref(),
                stream_name,
                &redact_log_value(&line),
            );
        }
    })
}

fn append_helper_log(path: Option<&Path>, stream: &str, line: &str) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = prepare_managed_log_for_append(path) {
        tracing::warn!(%error, path = %path.display(), "failed to prepare elevated helper log");
        return;
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut file) => {
            let _ = writeln!(
                file,
                "[{}][{stream}] {line}",
                format_current_log_timestamp()
            );
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to write elevated helper log");
        }
    }
}

#[cfg(windows)]
fn assign_child_to_kill_on_close_job(child: &std::process::Child) -> Option<JobHandle> {
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        tracing::warn!("failed to create elevated helper job object");
        return None;
    }
    let handle = JobHandle(job as isize);
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        tracing::warn!("failed to configure elevated helper job object");
    }
    let assigned = unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as _) };
    if assigned == 0 {
        tracing::warn!("failed to assign mihomo to elevated helper job object");
    }
    Some(handle)
}

#[cfg(not(windows))]
fn assign_child_to_kill_on_close_job(_child: &std::process::Child) -> Option<()> {
    None
}

#[cfg(target_os = "macos")]
fn read_pid_file(pid_file: &Path) -> AppResult<u32> {
    let source = std::fs::read_to_string(pid_file).map_err(|error| {
        PlatformError::OperationFailed(format!("读取提权核心 PID 文件失败: {error}"))
    })?;
    source.trim().parse::<u32>().map_err(|error| {
        PlatformError::OperationFailed(format!("提权核心 PID 格式无效: {error}")).into()
    })
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MacosSignal {
    Probe,
    Terminate,
    Kill,
}

#[cfg(target_os = "macos")]
impl MacosSignal {
    fn number(self) -> i32 {
        match self {
            Self::Probe => 0,
            Self::Terminate => 15,
            Self::Kill => 9,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Probe => "0",
            Self::Terminate => "TERM",
            Self::Kill => "KILL",
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignalOutcome {
    Sent,
    NoSuchProcess,
    PermissionDenied,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MacosProcessProbe {
    Running,
    Exited,
}

#[cfg(target_os = "macos")]
fn signal_process(pid: u32, signal: MacosSignal) -> AppResult<SignalOutcome> {
    const EPERM: i32 = 1;
    const ESRCH: i32 = 3;

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let pid = i32::try_from(pid).map_err(|_| {
        PlatformError::OperationFailed(format!("提权核心 PID 超出 macOS 进程号范围: {pid}"))
    })?;
    let result = unsafe { kill(pid, signal.number()) };
    if result == 0 {
        return Ok(SignalOutcome::Sent);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(EPERM) => Ok(SignalOutcome::PermissionDenied),
        Some(ESRCH) => Ok(SignalOutcome::NoSuchProcess),
        _ => Err(PlatformError::OperationFailed(format!(
            "发送进程信号失败: kill -{} {pid}: {error}",
            signal.name()
        ))
        .into()),
    }
}

#[cfg(target_os = "macos")]
fn macos_process_probe(pid: u32) -> AppResult<MacosProcessProbe> {
    Ok(macos_process_probe_from_signal_outcome(signal_process(
        pid,
        MacosSignal::Probe,
    )?))
}

#[cfg(target_os = "macos")]
fn macos_process_probe_from_signal_outcome(outcome: SignalOutcome) -> MacosProcessProbe {
    match outcome {
        SignalOutcome::Sent | SignalOutcome::PermissionDenied => MacosProcessProbe::Running,
        SignalOutcome::NoSuchProcess => MacosProcessProbe::Exited,
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn macos_elevated_shell_script(config: &ElevatedProcessConfig, pid_file: &Path) -> String {
    let mut env_prefix = String::new();
    for (key, value) in &config.env {
        env_prefix.push_str("export ");
        env_prefix.push_str(key);
        env_prefix.push('=');
        env_prefix.push_str(&shell_quote(value));
        env_prefix.push_str("; ");
    }
    let args = config
        .args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let log_redirect = config
        .console_log_path
        .as_ref()
        .map(|path| format!(" >> {} 2>&1", shell_quote(&path.to_string_lossy())))
        .unwrap_or_else(|| " >/dev/null 2>&1".to_string());

    format!(
        "{route_cleanup} cd {cwd}; {env_prefix}{program} {args}{log_redirect} & echo $! > {pid_file}",
        route_cleanup = macos_stale_route_cleanup_shell_script(),
        cwd = shell_quote(&config.current_dir.to_string_lossy()),
        program = shell_quote(&config.program.to_string_lossy()),
        pid_file = shell_quote(&pid_file.to_string_lossy()),
    )
}

#[cfg(target_os = "macos")]
fn macos_stale_route_cleanup_shell_script() -> &'static str {
    "for air_route in 1.0.0.0/8 2.0.0.0/7 4.0.0.0/6 8.0.0.0/5 16.0.0.0/4 32.0.0.0/3 64.0.0.0/2 128.0.0.0/1 198.18.0.0/16; do route -n delete -net \"$air_route\" 198.18.0.3 >/dev/null 2>&1 || true; done; route -n delete -host 198.18.0.3 198.18.0.1 >/dev/null 2>&1 || true;"
}

#[cfg(target_os = "macos")]
fn macos_admin_osascript(shell_script: &str) -> String {
    format!(
        "do shell script {} with administrator privileges with prompt \"Air 需要管理员权限来启动或停止 mihomo TUN 模式。\"",
        applescript_quote(shell_script)
    )
}

#[cfg(target_os = "macos")]
fn run_macos_admin_osascript(
    apple_script: &str,
    action: &'static str,
) -> AppResult<std::process::ExitStatus> {
    let mut child = Command::new("osascript")
        .arg("-e")
        .arg(apple_script)
        .spawn()
        .map_err(|error| {
            PlatformError::OperationFailed(format!(
                "启动 macOS 管理员授权{action}核心失败: {error}"
            ))
        })?;
    let started = std::time::Instant::now();
    let timeout = macos_admin_authorization_timeout();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            PlatformError::OperationFailed(format!(
                "等待 macOS 管理员授权{action}核心失败: {error}"
            ))
        })? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PlatformError::OperationFailed(macos_admin_timeout_message(action)).into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(target_os = "macos")]
fn macos_admin_authorization_timeout() -> Duration {
    std::env::var("AIR_MACOS_ADMIN_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(180))
}

#[cfg(target_os = "macos")]
fn macos_admin_timeout_message(action: &str) -> String {
    format!(
        "等待 macOS 管理员授权{action}核心超时。如果系统授权弹窗没有出现，请先在终端运行 scripts/verify-macos-tun.sh --sudo 验证 TUN 权限。"
    )
}

#[cfg(target_os = "macos")]
fn macos_admin_signal_script(pid: u32, signal: MacosSignal) -> String {
    macos_admin_osascript(&format!("kill -{} {pid}", signal.name()))
}

#[cfg(target_os = "macos")]
fn send_macos_admin_signal(pid: u32, signal: MacosSignal) -> AppResult<()> {
    let apple_script = macos_admin_signal_script(pid, signal);
    let status = run_macos_admin_osascript(&apple_script, "停止")?;
    if !status.success() {
        return Err(PlatformError::OperationFailed(format!(
            "macOS 管理员授权被取消或停止核心失败，osascript={:?}。如果系统授权弹窗没有出现，请在终端运行 scripts/verify-macos-tun.sh --sudo 验证 TUN 权限。",
            status.code()
        ))
        .into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn applescript_quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(windows)]
struct JobHandle(isize);

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(
                self.0 as windows_sys::Win32::Foundation::HANDLE,
            );
        }
    }
}

#[cfg(windows)]
fn elevated_try_wait(child: &ElevatedChild) -> AppResult<Option<Option<i32>>> {
    const STILL_ACTIVE: u32 = 259;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;

    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(child.raw_handle(), &mut code) };
    if ok == 0 {
        return Err(PlatformError::OperationFailed(format!(
            "读取提权核心进程退出状态失败: {}",
            std::io::Error::last_os_error()
        ))
        .into());
    }
    if code == STILL_ACTIVE {
        Ok(None)
    } else {
        Ok(Some(Some(code as i32)))
    }
}

#[cfg(target_os = "macos")]
fn elevated_try_wait(child: &ElevatedChild) -> AppResult<Option<Option<i32>>> {
    match macos_process_probe(child.pid)? {
        MacosProcessProbe::Running => Ok(None),
        MacosProcessProbe::Exited => Ok(Some(None)),
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn elevated_try_wait(_child: &ElevatedChild) -> AppResult<Option<Option<i32>>> {
    Err(PlatformError::Unsupported("当前平台不支持提权进程状态读取".into()).into())
}

#[cfg(windows)]
fn elevated_wait_timeout(
    child: &ElevatedChild,
    timeout: Duration,
) -> AppResult<Option<Option<i32>>> {
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    let wait = unsafe { WaitForSingleObject(child.raw_handle(), timeout_ms) };
    match wait {
        WAIT_OBJECT_0 => child.try_wait(),
        WAIT_TIMEOUT => Ok(None),
        WAIT_FAILED => Err(PlatformError::OperationFailed(format!(
            "等待提权核心进程失败: {}",
            std::io::Error::last_os_error()
        ))
        .into()),
        other => Err(PlatformError::OperationFailed(format!(
            "等待提权核心进程返回未知状态: {other}"
        ))
        .into()),
    }
}

#[cfg(target_os = "macos")]
fn elevated_wait_timeout(
    child: &ElevatedChild,
    timeout: Duration,
) -> AppResult<Option<Option<i32>>> {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(None)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn elevated_wait_timeout(
    _child: &ElevatedChild,
    _timeout: Duration,
) -> AppResult<Option<Option<i32>>> {
    Err(PlatformError::Unsupported("当前平台不支持等待提权进程".into()).into())
}

#[cfg(windows)]
fn elevated_kill(child: &ElevatedChild) -> AppResult<()> {
    use windows_sys::Win32::System::Threading::TerminateProcess;

    let ok = unsafe { TerminateProcess(child.raw_handle(), 1) };
    if ok == 0 {
        return Err(PlatformError::OperationFailed(format!(
            "结束提权核心进程失败: {}",
            std::io::Error::last_os_error()
        ))
        .into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn elevated_kill(child: &ElevatedChild) -> AppResult<()> {
    let result = terminate_macos_pid(child.pid, Duration::from_secs(2));
    let _ = child.pid_file.as_ref().map(std::fs::remove_file);
    result
}

#[cfg(target_os = "macos")]
fn terminate_macos_pid(pid: u32, timeout: Duration) -> AppResult<()> {
    match signal_process(pid, MacosSignal::Terminate)? {
        SignalOutcome::Sent => {}
        SignalOutcome::NoSuchProcess => {
            return Ok(());
        }
        SignalOutcome::PermissionDenied => {
            send_macos_admin_signal(pid, MacosSignal::Terminate)?;
        }
    };
    if wait_macos_pid_exit(pid, timeout)?.is_none() {
        match signal_process(pid, MacosSignal::Kill)? {
            SignalOutcome::Sent | SignalOutcome::NoSuchProcess => {}
            SignalOutcome::PermissionDenied => {
                send_macos_admin_signal(pid, MacosSignal::Kill)?;
            }
        }
        let _ = wait_macos_pid_exit(pid, Duration::from_secs(2))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn wait_macos_pid_exit(pid: u32, timeout: Duration) -> AppResult<Option<()>> {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if matches!(macos_process_probe(pid)?, MacosProcessProbe::Exited) {
            return Ok(Some(()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(None)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn elevated_kill(_child: &ElevatedChild) -> AppResult<()> {
    Err(PlatformError::Unsupported("当前平台不支持结束提权进程".into()).into())
}

#[cfg(windows)]
fn wide_os_null(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevated_core_helper_args_roundtrip() {
        let config = ElevatedProcessConfig {
            program: PathBuf::from(r"C:\Air\mihomo.exe"),
            args: vec!["-d".into(), r"C:\Air Config".into()],
            env: BTreeMap::from([("SAFE_PATHS".into(), r"C:\Air Config".into())]),
            current_dir: PathBuf::from(r"C:\Air Config"),
            console_log_path: Some(PathBuf::from(r"C:\Air Logs\core.log")),
        };

        let parsed = parse_elevated_core_helper_args(elevated_core_helper_args(&config)).unwrap();

        assert_eq!(parsed.program, config.program);
        assert_eq!(parsed.args, config.args);
        assert_eq!(parsed.env, config.env);
        assert_eq!(parsed.current_dir, config.current_dir);
        assert_eq!(parsed.console_log_path, config.console_log_path);
    }

    #[test]
    fn helper_rejects_unknown_args() {
        let result = parse_elevated_core_helper_args(["--bad".to_string()]);

        assert!(result.is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_elevated_script_quotes_paths_args_and_env() {
        let config = ElevatedProcessConfig {
            program: PathBuf::from("/tmp/Air Test/mihomo's core"),
            args: vec![
                "-d".into(),
                "/tmp/Air Data".into(),
                "-f".into(),
                "a'b.yaml".into(),
            ],
            env: BTreeMap::from([("SAFE_PATHS".into(), "/tmp/Air Data:/tmp/Other".into())]),
            current_dir: PathBuf::from("/tmp/Air Data"),
            console_log_path: Some(PathBuf::from("/tmp/Air Logs/core.log")),
        };

        let script = macos_elevated_shell_script(&config, Path::new("/tmp/air-core.pid"));

        assert!(script.contains("cd '/tmp/Air Data'"));
        assert!(script.contains("export SAFE_PATHS='/tmp/Air Data:/tmp/Other';"));
        assert!(script.contains("'/tmp/Air Test/mihomo'\\''s core'"));
        assert!(script.contains("'a'\\''b.yaml'"));
        assert!(script.contains("echo $! > '/tmp/air-core.pid'"));
        assert!(!script.contains("nohup"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_elevated_script_cleans_stale_mihomo_routes_before_starting_core() {
        let config = ElevatedProcessConfig {
            program: PathBuf::from("/tmp/mihomo"),
            args: vec!["-d".into(), "/tmp/core".into()],
            env: BTreeMap::new(),
            current_dir: PathBuf::from("/tmp/core"),
            console_log_path: None,
        };

        let script = macos_elevated_shell_script(&config, Path::new("/tmp/air-core.pid"));
        let cleanup = macos_stale_route_cleanup_shell_script();
        let cleanup_pos = script
            .find(&cleanup)
            .expect("elevated launch script should clean stale Air routes");
        let program_pos = script
            .find("'/tmp/mihomo'")
            .expect("elevated launch script should start mihomo");

        assert!(cleanup_pos < program_pos);
        assert!(cleanup.contains("198.18.0.3"));
        assert!(cleanup.contains("route -n delete -net"));
        assert!(cleanup.contains("route -n delete -host 198.18.0.3"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_admin_osascript_escapes_shell_script_for_applescript() {
        let command = macos_admin_osascript(r#"echo "hi" \ done"#);

        assert_eq!(
            command,
            r#"do shell script "echo \"hi\" \\ done" with administrator privileges with prompt "Air 需要管理员权限来启动或停止 mihomo TUN 模式。""#
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_admin_timeout_message_points_to_terminal_sudo_verifier() {
        let message = macos_admin_timeout_message("启动");

        assert!(message.contains("等待 macOS 管理员授权启动核心超时"));
        assert!(message.contains("scripts/verify-macos-tun.sh --sudo"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_process_probe_treats_permission_denied_as_running() {
        assert_eq!(
            macos_process_probe_from_signal_outcome(SignalOutcome::PermissionDenied),
            MacosProcessProbe::Running
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_admin_signal_script_targets_pid_and_signal() {
        assert_eq!(
            macos_admin_signal_script(42, MacosSignal::Terminate),
            r#"do shell script "kill -TERM 42" with administrator privileges with prompt "Air 需要管理员权限来启动或停止 mihomo TUN 模式。""#
        );
        assert_eq!(
            macos_admin_signal_script(42, MacosSignal::Kill),
            r#"do shell script "kill -KILL 42" with administrator privileges with prompt "Air 需要管理员权限来启动或停止 mihomo TUN 模式。""#
        );
    }
}
