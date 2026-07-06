use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast};

use air_error::{AppResult, ProcessError};
use air_mihomo::{CoreBinary, CoreLaunchOptions, CoreProcessStatus, CoreRuntime};
use air_platform::core_service;
#[cfg(target_os = "macos")]
use air_platform::elevated_process::terminate_macos_process_with_admin;
use air_platform::elevated_process::{ElevatedChild, ElevatedProcessConfig};
use air_platform::privilege;
use air_telemetry::log_retention::{format_current_log_timestamp, prepare_managed_log_for_append};
use air_telemetry::redaction::redact_log_value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessLaunchConfig {
    pub binary_path: PathBuf,
    pub config_path: PathBuf,
    pub working_dir: PathBuf,
    pub console_log_path: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub safe_paths: Vec<PathBuf>,
    pub requires_admin: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPreview {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub current_dir: PathBuf,
    pub requires_admin: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProcessEvent {
    Log {
        stream: ProcessLogStream,
        line: String,
    },
    Exited {
        status: Option<i32>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProcessLogStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct ProcessInner {
    status: CoreProcessStatus,
    child: Option<ManagedChild>,
    last_error: Option<String>,
}

#[derive(Debug)]
enum ManagedChild {
    Tokio(TokioManagedChild),
    Elevated(ElevatedChild),
    Service,
}

#[derive(Debug)]
struct TokioManagedChild {
    process: Option<Child>,
    // Windows 下把普通 mihomo 子进程放进 kill-on-close JobObject。
    // 这样 GUI 被任务管理器强杀时，系统关闭 job 句柄也会连带结束内核进程。
    // mihomo /restart 在 Windows 会先拉起新进程再退出旧进程；旧 Child 退出后仍要保留
    // JobObject 到真正 StopCore/应用退出，否则新进程可能因 kill-on-close 被误杀。
    _job: Option<JobHandle>,
}

#[cfg(windows)]
#[derive(Debug)]
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

#[cfg(not(windows))]
type JobHandle = ();

#[derive(Clone, Debug)]
pub struct MihomoProcessManager {
    inner: Arc<Mutex<ProcessInner>>,
    events: broadcast::Sender<ProcessEvent>,
    stop_timeout: Duration,
}

impl Default for MihomoProcessManager {
    fn default() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Mutex::new(ProcessInner {
                status: CoreProcessStatus::Stopped,
                child: None,
                last_error: None,
            })),
            events,
            stop_timeout: Duration::from_millis(500),
        }
    }
}

impl MihomoProcessManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProcessEvent> {
        self.events.subscribe()
    }

    pub fn build_command_preview(config: &ProcessLaunchConfig) -> CommandPreview {
        let mut env = config.env.clone();
        if !config.safe_paths.is_empty() {
            env.insert("SAFE_PATHS".into(), join_safe_paths(&config.safe_paths));
        }
        CommandPreview {
            program: config.binary_path.clone(),
            args: vec![
                "-d".into(),
                config.working_dir.to_string_lossy().to_string(),
                "-f".into(),
                config.config_path.to_string_lossy().to_string(),
            ],
            env,
            current_dir: config.working_dir.clone(),
            requires_admin: config.requires_admin,
        }
    }

    pub async fn start_with_config(
        &self,
        config: ProcessLaunchConfig,
    ) -> AppResult<CoreProcessStatus> {
        let mut inner = self.inner.lock().await;
        reap_exited_child(&mut inner, &self.events)?;
        if matches!(
            inner.status,
            CoreProcessStatus::Starting | CoreProcessStatus::Running { .. }
        ) {
            // UI 启动阶段可能同时收到自动启动、托盘启动或按钮连点；对同一个受管核心而言，
            // “再次启动”应视为幂等操作，避免把正常竞态暴露成用户可见错误。
            tracing::info!(
                status = ?inner.status,
                "mihomo process start requested while already managed"
            );
            return Ok(inner.status.clone());
        }
        if matches!(inner.status, CoreProcessStatus::Stopping) {
            return Err(ProcessError::InvalidState("mihomo 正在停止，稍后再启动".into()).into());
        }
        if !config.binary_path.is_file() {
            return Err(ProcessError::BinaryNotFound(config.binary_path).into());
        }
        prepare_console_log_file(config.console_log_path.as_deref())?;
        let preview = Self::build_command_preview(&config);
        let should_elevate = preview.requires_admin && !privilege::current_process_is_elevated()?;
        tracing::info!(
            program = %preview.program.display(),
            current_dir = %preview.current_dir.display(),
            args = ?preview.args,
            env = ?preview.env,
            elevated = should_elevate,
            "spawning mihomo process"
        );
        if should_elevate {
            if core_service::query_core_service()?.installed {
                inner.status = CoreProcessStatus::Starting;
                tracing::info!("starting mihomo through installed core service");
                core_service::start_core_service()?;
                inner.child = Some(ManagedChild::Service);
                inner.status = CoreProcessStatus::Running { pid: 0 };
                return Ok(inner.status.clone());
            }
            if let Some(child) =
                adopt_existing_elevated_mihomo_process(&preview, config.console_log_path.as_deref())
            {
                let pid = child.id();
                inner.child = Some(ManagedChild::Elevated(child));
                inner.status = CoreProcessStatus::Running { pid };
                return Ok(inner.status.clone());
            }
            cleanup_stale_elevated_mihomo_processes(&preview, config.console_log_path.as_deref())?;
            inner.status = CoreProcessStatus::Starting;
            let child = match spawn_elevated_child(&preview, config.console_log_path.clone()) {
                Ok(child) => child,
                Err(error) => {
                    inner.status = CoreProcessStatus::Failed {
                        message: error.to_string(),
                    };
                    inner.last_error = Some(error.to_string());
                    tracing::warn!(
                        error = %error,
                        "failed to spawn elevated mihomo process"
                    );
                    return Err(error);
                }
            };
            let pid = child.id();
            tracing::info!(pid, "elevated mihomo process spawned");
            let message = "mihomo 已通过管理员权限启动，stdout/stderr 将写入 core.log";
            append_console_log_line_if_configured(config.console_log_path.as_deref(), message);
            let _ = self.events.send(ProcessEvent::Log {
                stream: ProcessLogStream::Stdout,
                line: message.to_string(),
            });
            inner.child = Some(ManagedChild::Elevated(child));
            inner.status = CoreProcessStatus::Running { pid };
            return Ok(inner.status.clone());
        }
        inner.status = CoreProcessStatus::Starting;
        let mut command = Command::new(&preview.program);
        command
            .args(&preview.args)
            .current_dir(&preview.current_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        air_platform::process::hide_tokio_subprocess_window(&mut command);
        for (key, value) in &preview.env {
            command.env(key, value);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                inner.status = CoreProcessStatus::Failed {
                    message: error.to_string(),
                };
                inner.last_error = Some(error.to_string());
                tracing::warn!(
                    error = %error,
                    "failed to spawn mihomo process"
                );
                return Err(ProcessError::Io(error).into());
            }
        };
        let pid = child.id().unwrap_or_default();
        tracing::info!(pid, "mihomo process spawned");
        let child_job = assign_tokio_child_to_kill_on_close_job(&child, "mihomo process");
        attach_log_reader(
            child.stdout.take(),
            ProcessLogStream::Stdout,
            self.events.clone(),
            config.console_log_path.clone(),
        );
        attach_log_reader(
            child.stderr.take(),
            ProcessLogStream::Stderr,
            self.events.clone(),
            config.console_log_path.clone(),
        );
        inner.child = Some(ManagedChild::Tokio(TokioManagedChild {
            process: Some(child),
            _job: child_job,
        }));
        inner.status = CoreProcessStatus::Running { pid };
        Ok(inner.status.clone())
    }

    pub async fn stop_with_timeout(&self, timeout: Duration) -> AppResult<CoreProcessStatus> {
        tracing::info!(timeout_ms = timeout.as_millis(), "stopping mihomo process");
        let mut child = {
            let mut inner = self.inner.lock().await;
            inner.status = CoreProcessStatus::Stopping;
            inner.child.take()
        };
        if let Some(mut child) = child.take() {
            // mihomo 没有跨平台统一的“温和退出”控制面；Windows 通常只能 kill，
            // Unix 后续可切到 SIGTERM。当前先等待短窗口，再强制结束，避免 UI 卡死。
            stop_managed_child(&mut child, timeout, &self.events).await?;
        } else {
            tracing::debug!("mihomo process stop requested without running child");
        }
        let mut inner = self.inner.lock().await;
        inner.status = CoreProcessStatus::Stopped;
        Ok(inner.status.clone())
    }
}

#[async_trait]
impl CoreRuntime for MihomoProcessManager {
    async fn detect(&self, search_dir: &std::path::Path) -> AppResult<Option<CoreBinary>> {
        let binary = search_dir.join(platform_binary_name());
        Ok(binary.is_file().then_some(CoreBinary {
            path: binary,
            version: None,
        }))
    }

    async fn start(&self, options: CoreLaunchOptions) -> AppResult<CoreProcessStatus> {
        self.start_with_config(ProcessLaunchConfig {
            binary_path: options.binary.path,
            config_path: options.config_path,
            working_dir: options.working_dir,
            console_log_path: None,
            env: BTreeMap::new(),
            safe_paths: Vec::new(),
            requires_admin: false,
        })
        .await
    }

    async fn stop(&self) -> AppResult<CoreProcessStatus> {
        self.stop_with_timeout(self.stop_timeout).await
    }

    async fn restart(&self, options: CoreLaunchOptions) -> AppResult<CoreProcessStatus> {
        let _ = self.stop().await?;
        self.start(options).await
    }

    async fn status(&self) -> AppResult<CoreProcessStatus> {
        self.refresh_status_from_child().await
    }
}

#[async_trait]
pub trait ProcessControl: Send + Sync {
    async fn start_process(&self, config: ProcessLaunchConfig) -> AppResult<CoreProcessStatus>;
    async fn stop_process(&self) -> AppResult<CoreProcessStatus>;
    async fn restart_process(&self, config: ProcessLaunchConfig) -> AppResult<CoreProcessStatus>;
    async fn process_status(&self) -> AppResult<CoreProcessStatus>;
}

#[async_trait]
impl ProcessControl for MihomoProcessManager {
    async fn start_process(&self, config: ProcessLaunchConfig) -> AppResult<CoreProcessStatus> {
        self.start_with_config(config).await
    }

    async fn stop_process(&self) -> AppResult<CoreProcessStatus> {
        self.stop().await
    }

    async fn restart_process(&self, config: ProcessLaunchConfig) -> AppResult<CoreProcessStatus> {
        let _ = self.stop_process().await?;
        self.start_process(config).await
    }

    async fn process_status(&self) -> AppResult<CoreProcessStatus> {
        self.refresh_status_from_child().await
    }
}

impl MihomoProcessManager {
    async fn refresh_status_from_child(&self) -> AppResult<CoreProcessStatus> {
        let mut inner = self.inner.lock().await;
        reap_exited_child(&mut inner, &self.events)?;
        tracing::debug!(status = ?inner.status, "refreshed mihomo process status from child handle");
        Ok(inner.status.clone())
    }
}

fn spawn_elevated_child(
    preview: &CommandPreview,
    console_log_path: Option<PathBuf>,
) -> AppResult<ElevatedChild> {
    tracing::info!(
        program = %preview.program.display(),
        args = ?preview.args,
        env = ?preview.env,
        current_dir = %preview.current_dir.display(),
        log_path = ?console_log_path,
        "spawning elevated mihomo child"
    );
    ElevatedChild::spawn(&ElevatedProcessConfig {
        program: preview.program.clone(),
        args: preview.args.clone(),
        env: preview.env.clone(),
        current_dir: preview.current_dir.clone(),
        console_log_path,
    })
}

#[cfg(target_os = "macos")]
fn adopt_existing_elevated_mihomo_process(
    preview: &CommandPreview,
    console_log_path: Option<&Path>,
) -> Option<ElevatedChild> {
    let pid = macos_select_adoptable_core_pid(
        preview,
        std::process::id(),
        list_macos_process_ids(),
        |pid| macos_core_process_matches_preview(pid, preview),
    )?;
    tracing::info!(
        pid,
        program = %preview.program.display(),
        args = ?preview.args,
        "adopting already running elevated mihomo process"
    );
    append_console_log_line_if_configured(
        console_log_path,
        &format!("Air 检测到已运行的 mihomo 进程 {pid}，已接管而不是重复启动"),
    );
    Some(ElevatedChild::adopt_macos_pid(pid))
}

#[cfg(not(target_os = "macos"))]
fn adopt_existing_elevated_mihomo_process(
    _preview: &CommandPreview,
    _console_log_path: Option<&Path>,
) -> Option<ElevatedChild> {
    None
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct MacosAirMihomoPidFile {
    pid_file: PathBuf,
    owner_pid: u32,
    core_pid: u32,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MacosCoreProcessMatch {
    Matches,
    DoesNotMatch,
    Unknown,
}

#[cfg(target_os = "macos")]
fn cleanup_stale_elevated_mihomo_processes(
    preview: &CommandPreview,
    console_log_path: Option<&Path>,
) -> AppResult<()> {
    let current_pid = std::process::id();
    for candidate in discover_macos_air_mihomo_pid_files() {
        if !macos_pid_is_running(candidate.core_pid) {
            let _ = std::fs::remove_file(&candidate.pid_file);
            continue;
        }
        let should_cleanup = macos_stale_pid_file_should_cleanup(
            &candidate,
            preview,
            current_pid,
            macos_pid_is_running,
            |pid| !macos_pid_is_running(pid),
            |pid| macos_core_process_matches_preview(pid, preview),
        );
        if !should_cleanup {
            continue;
        }

        tracing::warn!(
            owner_pid = candidate.owner_pid,
            core_pid = candidate.core_pid,
            pid_file = %candidate.pid_file.display(),
            "terminating stale elevated mihomo process before starting a new one"
        );
        append_console_log_line_if_configured(
            console_log_path,
            &format!(
                "Air 检测到旧的提权 mihomo 进程 {}，正在启动前清理",
                candidate.core_pid
            ),
        );
        terminate_macos_process_with_admin(candidate.core_pid, Duration::from_secs(2))?;
        let _ = std::fs::remove_file(&candidate.pid_file);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn cleanup_stale_elevated_mihomo_processes(
    _preview: &CommandPreview,
    _console_log_path: Option<&Path>,
) -> AppResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn discover_macos_air_mihomo_pid_files() -> Vec<MacosAirMihomoPidFile> {
    let mut candidates = Vec::new();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return candidates;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(candidate) = parse_macos_air_mihomo_pid_file(&path, &source) {
            candidates.push(candidate);
        }
    }
    candidates
}

#[cfg(target_os = "macos")]
fn parse_macos_air_mihomo_pid_file(path: &Path, source: &str) -> Option<MacosAirMihomoPidFile> {
    let file_name = path.file_name()?.to_str()?;
    let owner_pid = file_name
        .strip_prefix("air-mihomo-")?
        .strip_suffix(".pid")?
        .parse::<u32>()
        .ok()?;
    let core_pid = source.trim().parse::<u32>().ok()?;
    Some(MacosAirMihomoPidFile {
        pid_file: path.to_path_buf(),
        owner_pid,
        core_pid,
    })
}

#[cfg(target_os = "macos")]
fn macos_stale_pid_file_should_cleanup(
    candidate: &MacosAirMihomoPidFile,
    _preview: &CommandPreview,
    current_pid: u32,
    core_is_running: impl Fn(u32) -> bool,
    owner_is_dead: impl Fn(u32) -> bool,
    core_matches: impl Fn(u32) -> MacosCoreProcessMatch,
) -> bool {
    if candidate.owner_pid == current_pid {
        return false;
    }
    if !core_is_running(candidate.core_pid) {
        return false;
    }
    if !owner_is_dead(candidate.owner_pid) {
        return false;
    }
    matches!(
        core_matches(candidate.core_pid),
        MacosCoreProcessMatch::Matches
    )
}

#[cfg(target_os = "macos")]
fn macos_select_adoptable_core_pid(
    _preview: &CommandPreview,
    current_pid: u32,
    pids: impl IntoIterator<Item = u32>,
    core_matches: impl Fn(u32) -> MacosCoreProcessMatch,
) -> Option<u32> {
    pids.into_iter()
        .filter(|pid| *pid != current_pid)
        .find(|pid| matches!(core_matches(*pid), MacosCoreProcessMatch::Matches))
}

#[cfg(target_os = "macos")]
fn list_macos_process_ids() -> Vec<u32> {
    match std::fs::read_dir("/proc") {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .collect(),
        Err(_) => list_macos_process_ids_with_ps(),
    }
}

#[cfg(target_os = "macos")]
fn list_macos_process_ids_with_ps() -> Vec<u32> {
    std::process::Command::new("ps")
        .args(["-axo", "pid="])
        .output()
        .ok()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn macos_pid_is_running(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(target_os = "macos")]
fn macos_core_process_matches_preview(pid: u32, preview: &CommandPreview) -> MacosCoreProcessMatch {
    match read_macos_process_argv(pid) {
        Ok(Some(argv)) => {
            if macos_process_argv_matches_preview(&argv, preview) {
                MacosCoreProcessMatch::Matches
            } else {
                MacosCoreProcessMatch::DoesNotMatch
            }
        }
        Ok(None) => MacosCoreProcessMatch::Unknown,
        Err(error) => {
            tracing::debug!(
                pid,
                error = %error,
                "failed to read macOS process argv through sysctl; trying ps fallback"
            );
            match read_macos_process_command_with_ps(pid) {
                Ok(Some(command)) => {
                    if macos_process_command_matches_preview(&command, preview) {
                        MacosCoreProcessMatch::Matches
                    } else {
                        MacosCoreProcessMatch::DoesNotMatch
                    }
                }
                Ok(None) | Err(_) => MacosCoreProcessMatch::Unknown,
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_process_argv_matches_preview(argv: &[String], preview: &CommandPreview) -> bool {
    argv.first()
        .map(|program| Path::new(program) == preview.program)
        .unwrap_or(false)
        && argv
            .get(1..)
            .map(|args| args == preview.args)
            .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn read_macos_process_command_with_ps(pid: u32) -> std::io::Result<Option<String>> {
    let output = std::process::Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!command.is_empty()).then_some(command))
}

#[cfg(target_os = "macos")]
fn macos_process_command_matches_preview(command: &str, preview: &CommandPreview) -> bool {
    command.trim() == macos_preview_command_line(preview)
}

#[cfg(target_os = "macos")]
fn macos_preview_command_line(preview: &CommandPreview) -> String {
    std::iter::once(preview.program.to_string_lossy().to_string())
        .chain(preview.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "macos")]
fn read_macos_process_argv(pid: u32) -> std::io::Result<Option<Vec<String>>> {
    const CTL_KERN: i32 = 1;
    const KERN_PROCARGS2: i32 = 49;
    const ESRCH: i32 = 3;

    unsafe extern "C" {
        fn sysctl(
            name: *mut i32,
            namelen: u32,
            oldp: *mut std::ffi::c_void,
            oldlenp: *mut usize,
            newp: *mut std::ffi::c_void,
            newlen: usize,
        ) -> i32;
    }

    let pid = i32::try_from(pid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "pid out of range"))?;
    let mut mib = [CTL_KERN, KERN_PROCARGS2, pid];
    let mut size = 0usize;
    let result = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    if size == 0 {
        return Ok(None);
    }

    let mut buffer = vec![0u8; size];
    let result = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    buffer.truncate(size);
    Ok(parse_macos_procargs(&buffer))
}

#[cfg(target_os = "macos")]
fn parse_macos_procargs(buffer: &[u8]) -> Option<Vec<String>> {
    let argc_bytes: [u8; 4] = buffer.get(..4)?.try_into().ok()?;
    let argc = i32::from_ne_bytes(argc_bytes);
    if argc <= 0 {
        return None;
    }
    let mut offset = 4;
    while offset < buffer.len() && buffer[offset] != 0 {
        offset += 1;
    }
    while offset < buffer.len() && buffer[offset] == 0 {
        offset += 1;
    }

    let mut argv = Vec::new();
    for _ in 0..argc {
        if offset >= buffer.len() {
            break;
        }
        let start = offset;
        while offset < buffer.len() && buffer[offset] != 0 {
            offset += 1;
        }
        if offset > start {
            argv.push(String::from_utf8_lossy(&buffer[start..offset]).to_string());
        }
        while offset < buffer.len() && buffer[offset] == 0 {
            offset += 1;
        }
    }
    if argv.is_empty() { None } else { Some(argv) }
}

#[cfg(windows)]
fn assign_tokio_child_to_kill_on_close_job(
    child: &tokio::process::Child,
    label: &'static str,
) -> Option<JobHandle> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        tracing::warn!(label, "failed to create kill-on-close job object");
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
        tracing::warn!(label, "failed to configure kill-on-close job object");
    }
    let Some(raw_handle) = child.raw_handle() else {
        tracing::warn!(
            label,
            "tokio child has no raw handle for kill-on-close job object"
        );
        return Some(handle);
    };
    let assigned = unsafe { AssignProcessToJobObject(job, raw_handle) };
    if assigned == 0 {
        tracing::warn!(
            label,
            "failed to assign process to kill-on-close job object"
        );
    }
    Some(handle)
}

#[cfg(not(windows))]
fn assign_tokio_child_to_kill_on_close_job(
    _child: &tokio::process::Child,
    _label: &'static str,
) -> Option<JobHandle> {
    None
}

async fn stop_managed_child(
    child: &mut ManagedChild,
    timeout: Duration,
    events: &broadcast::Sender<ProcessEvent>,
) -> AppResult<()> {
    tracing::info!(child = ?child, timeout_ms = timeout.as_millis(), "stopping managed mihomo child");
    match child {
        ManagedChild::Tokio(child) => {
            if let Some(process) = child.process.as_mut() {
                stop_tokio_child(process, timeout, events).await
            } else {
                // /restart 后旧 Child 句柄可能已回收；此时保留的 JobObject 会在 ManagedChild
                // 被 drop 时关闭，确保最终停止仍能覆盖 mihomo 自行拉起的新进程。
                let _ = events.send(ProcessEvent::Exited { status: Some(0) });
                Ok(())
            }
        }
        ManagedChild::Elevated(child) => stop_elevated_child(child, timeout, events),
        ManagedChild::Service => stop_service_child(events),
    }
}

async fn stop_tokio_child(
    child: &mut Child,
    timeout: Duration,
    events: &broadcast::Sender<ProcessEvent>,
) -> AppResult<()> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            tracing::info!(
                status = status.code(),
                "mihomo process exited before stop timeout"
            );
            let _ = events.send(ProcessEvent::Exited {
                status: status.code(),
            });
        }
        Ok(Err(error)) => {
            tracing::warn!(
                error = %error,
                "failed while waiting mihomo process exit"
            );
            return Err(ProcessError::Io(error).into());
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "mihomo process did not exit in time; killing"
            );
            child.start_kill().map_err(ProcessError::Io)?;
            let status = child.wait().await.map_err(ProcessError::Io)?;
            tracing::info!(
                status = status.code(),
                "mihomo process killed after stop timeout"
            );
            let _ = events.send(ProcessEvent::Exited {
                status: status.code(),
            });
        }
    }
    Ok(())
}

fn stop_elevated_child(
    child: &ElevatedChild,
    timeout: Duration,
    events: &broadcast::Sender<ProcessEvent>,
) -> AppResult<()> {
    match child.wait_timeout(timeout)? {
        Some(status) => {
            tracing::info!(status, "elevated mihomo process exited before stop timeout");
            let _ = events.send(ProcessEvent::Exited { status });
        }
        None => {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "elevated mihomo process did not exit in time; killing"
            );
            child.kill()?;
            let status = child.wait_timeout(Duration::from_secs(2))?.flatten();
            tracing::info!(status, "elevated mihomo process killed after stop timeout");
            let _ = events.send(ProcessEvent::Exited { status });
        }
    }
    Ok(())
}

fn attach_log_reader<T>(
    stream: Option<T>,
    log_stream: ProcessLogStream,
    events: broadcast::Sender<ProcessEvent>,
    console_log_path: Option<PathBuf>,
) where
    T: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(stream) = stream else {
        tracing::debug!(stream = ?log_stream, "mihomo log reader skipped because stream is unavailable");
        return;
    };
    tokio::spawn(async move {
        tracing::info!(stream = ?log_stream, "mihomo log reader started");
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = redact_log_value(&line);
            if let Some(path) = console_log_path.as_ref() {
                if let Err(error) = append_console_log_line(path, log_stream, &line) {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        "failed to append mihomo console log"
                    );
                }
            }
            let _ = events.send(ProcessEvent::Log {
                stream: log_stream,
                line,
            });
        }
        tracing::info!(stream = ?log_stream, "mihomo log reader stopped");
    });
}

fn reap_exited_child(
    inner: &mut ProcessInner,
    events: &broadcast::Sender<ProcessEvent>,
) -> AppResult<()> {
    let Some(child) = inner.child.as_mut() else {
        return Ok(());
    };
    let Some(code) = try_wait_managed_child(child)? else {
        return Ok(());
    };

    tracing::info!(status = code, "reaped exited mihomo process");
    if matches!(child, ManagedChild::Tokio(_)) && code == Some(0) {
        // Windows /restart 会让旧进程退出 0，同时新进程继承当前 JobObject。这里不能丢弃
        // ManagedChild，否则 JobObject 关闭会把新进程一并杀掉；运行态由 controller 健康检查兜底。
        return Ok(());
    }
    inner.child = None;
    inner.status = if code == Some(0) {
        CoreProcessStatus::Stopped
    } else {
        CoreProcessStatus::Failed {
            message: format!("mihomo 已退出，状态码: {}", code.unwrap_or_default()),
        }
    };
    let _ = events.send(ProcessEvent::Exited { status: code });
    Ok(())
}

fn try_wait_managed_child(child: &mut ManagedChild) -> AppResult<Option<Option<i32>>> {
    match child {
        ManagedChild::Tokio(child) => {
            let Some(process) = child.process.as_mut() else {
                return Ok(None);
            };
            let Some(status) = process.try_wait().map_err(ProcessError::Io)? else {
                return Ok(None);
            };
            let code = status.code();
            let _ = child.process.take();
            Ok(Some(code))
        }
        ManagedChild::Elevated(child) => child.try_wait(),
        ManagedChild::Service => Ok(None),
    }
}

fn stop_service_child(events: &broadcast::Sender<ProcessEvent>) -> AppResult<()> {
    tracing::info!("stopping service-managed mihomo child");
    core_service::stop_core_service()?;
    let _ = events.send(ProcessEvent::Exited { status: Some(0) });
    Ok(())
}

fn prepare_console_log_file(path: Option<&Path>) -> AppResult<()> {
    let Some(path) = path else {
        return Ok(());
    };
    tracing::debug!(path = %path.display(), "preparing mihomo console log file");
    prepare_managed_log_for_append(path).map_err(ProcessError::Io)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(ProcessError::Io)?;
    Ok(())
}

fn append_console_log_line(
    path: &Path,
    stream: ProcessLogStream,
    line: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    prepare_managed_log_for_append(path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    // stdout/stderr 进入同一个 core.log；时间戳用于三天保留策略和人工排查，流向前缀用于区分普通日志和错误输出。
    file.write_all(
        format!(
            "[{}][{}] {line}\n",
            format_current_log_timestamp(),
            stream.as_str()
        )
        .as_bytes(),
    )
}

fn append_console_log_line_if_configured(path: Option<&Path>, line: &str) {
    let Some(path) = path else {
        return;
    };
    tracing::debug!(path = %path.display(), "appending elevated mihomo launch note");
    if let Err(error) = append_console_log_line(path, ProcessLogStream::Stdout, line) {
        tracing::warn!(
            %error,
            path = %path.display(),
            "failed to append elevated mihomo launch note"
        );
    }
}

impl ProcessLogStream {
    fn as_str(self) -> &'static str {
        match self {
            ProcessLogStream::Stdout => "stdout",
            ProcessLogStream::Stderr => "stderr",
        }
    }
}

fn join_safe_paths(paths: &[PathBuf]) -> String {
    let separator = if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    };
    let joined = paths
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(separator);
    tracing::debug!(safe_paths = %joined, "joined SAFE_PATHS for mihomo process");
    joined
}

fn platform_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "mihomo.exe"
    } else {
        "mihomo"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_preview_includes_working_dir_config_and_safe_paths() {
        let config = ProcessLaunchConfig {
            binary_path: PathBuf::from("mihomo"),
            config_path: PathBuf::from("config.yaml"),
            working_dir: PathBuf::from("work"),
            console_log_path: None,
            env: BTreeMap::from([("A".into(), "B".into())]),
            safe_paths: vec![PathBuf::from("profiles"), PathBuf::from("cache")],
            requires_admin: true,
        };

        let preview = MihomoProcessManager::build_command_preview(&config);

        assert_eq!(preview.args[0], "-d");
        assert!(preview.env.contains_key("SAFE_PATHS"));
        assert_eq!(preview.env["A"], "B");
        assert!(preview.requires_admin);
    }

    #[tokio::test]
    async fn duplicate_start_returns_existing_status_without_spawning_second_process() {
        let manager = MihomoProcessManager::new();
        {
            let mut inner = manager.inner.lock().await;
            inner.status = CoreProcessStatus::Running { pid: 1 };
        }
        let config = ProcessLaunchConfig {
            binary_path: PathBuf::from("missing"),
            config_path: PathBuf::from("config.yaml"),
            working_dir: PathBuf::from("."),
            console_log_path: None,
            env: BTreeMap::new(),
            safe_paths: Vec::new(),
            requires_admin: false,
        };

        let status = manager.start_with_config(config).await.unwrap();

        assert_eq!(status, CoreProcessStatus::Running { pid: 1 });
    }

    #[tokio::test]
    async fn start_while_stopping_is_rejected() {
        let manager = MihomoProcessManager::new();
        {
            let mut inner = manager.inner.lock().await;
            inner.status = CoreProcessStatus::Stopping;
        }
        let config = ProcessLaunchConfig {
            binary_path: PathBuf::from("missing"),
            config_path: PathBuf::from("config.yaml"),
            working_dir: PathBuf::from("."),
            console_log_path: None,
            env: BTreeMap::new(),
            safe_paths: Vec::new(),
            requires_admin: false,
        };

        let error = manager.start_with_config(config).await.unwrap_err();

        assert!(matches!(error, air_error::AppError::Process(_)));
    }

    #[tokio::test]
    async fn stop_from_no_child_returns_stopped() {
        let manager = MihomoProcessManager::new();

        let status = manager
            .stop_with_timeout(Duration::from_millis(1))
            .await
            .unwrap();

        assert_eq!(status, CoreProcessStatus::Stopped);
    }

    #[tokio::test]
    async fn stop_timeout_kills_running_child() {
        let manager = MihomoProcessManager::new();
        let child = spawn_long_running_child();
        {
            let mut inner = manager.inner.lock().await;
            inner.status = CoreProcessStatus::Running {
                pid: child.id().unwrap_or_default(),
            };
            inner.child = Some(ManagedChild::Tokio(TokioManagedChild {
                process: Some(child),
                _job: None,
            }));
        }

        let started = std::time::Instant::now();
        let status = manager
            .stop_with_timeout(Duration::from_millis(10))
            .await
            .unwrap();

        assert_eq!(status, CoreProcessStatus::Stopped);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "停止超时路径应快速强制结束子进程"
        );
    }

    #[tokio::test]
    async fn start_reaps_exited_child_before_rejecting_duplicate_start() {
        let temp = tempfile::tempdir().unwrap();
        let manager = MihomoProcessManager::new();
        let config = ProcessLaunchConfig {
            binary_path: shell_binary_path(),
            config_path: temp.path().join("config.yaml"),
            working_dir: temp.path().to_path_buf(),
            console_log_path: None,
            env: BTreeMap::new(),
            safe_paths: Vec::new(),
            requires_admin: false,
        };

        manager.start_with_config(config.clone()).await.unwrap();
        for _ in 0..40 {
            if !matches!(
                manager.status().await.unwrap(),
                CoreProcessStatus::Running { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let restarted = manager.start_with_config(config).await;

        assert!(
            restarted.is_ok(),
            "已退出的子进程不应继续阻塞下一次启动: {restarted:?}"
        );
        let _ = manager.stop_with_timeout(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn log_reader_appends_redacted_console_lines_to_core_log() {
        use tokio::io::AsyncWriteExt;

        let temp = tempfile::tempdir().unwrap();
        let log_path = temp.path().join("logs").join("core.log");
        let (events, mut receiver) = broadcast::channel(8);
        let (reader, mut writer) = tokio::io::duplex(128);

        attach_log_reader(
            Some(reader),
            ProcessLogStream::Stdout,
            events,
            Some(log_path.clone()),
        );

        writer
            .write_all(b"mihomo secret=abc started\n")
            .await
            .unwrap();
        drop(writer);
        let event = receiver.recv().await.unwrap();
        assert!(matches!(event, ProcessEvent::Log { .. }));

        let content = std::fs::read_to_string(log_path).unwrap();
        assert!(content.contains("[stdout] mihomo secret=*** started"));
        assert!(!content.contains("abc"));
    }

    fn spawn_long_running_child() -> Child {
        // 测试只需要一个可被 kill 的长任务，用系统 shell 避免依赖真实 mihomo。
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = Command::new("powershell");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 5"]);
            command
        };

        #[cfg(not(target_os = "windows"))]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 5"]);
            command
        };

        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("long-running child should spawn")
    }

    fn shell_binary_path() -> PathBuf {
        // 进程管理测试只验证状态机；这里借系统 shell 制造一个会因 mihomo 参数不兼容而快速退出的子进程。
        #[cfg(target_os = "windows")]
        {
            std::env::var("ComSpec")
                .map(PathBuf::from)
                .expect("ComSpec should point to cmd.exe on Windows")
        }

        #[cfg(not(target_os = "windows"))]
        {
            PathBuf::from("/bin/sh")
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_pid_file_parser_accepts_air_mihomo_pid_files_only() {
        let candidate =
            parse_macos_air_mihomo_pid_file(Path::new("/tmp/air-mihomo-123.pid"), "456\n")
                .expect("valid Air mihomo pid file should parse");

        assert_eq!(candidate.owner_pid, 123);
        assert_eq!(candidate.core_pid, 456);
        assert_eq!(candidate.pid_file, PathBuf::from("/tmp/air-mihomo-123.pid"));
        assert!(parse_macos_air_mihomo_pid_file(Path::new("/tmp/other.pid"), "456").is_none());
        assert!(
            parse_macos_air_mihomo_pid_file(Path::new("/tmp/air-mihomo-abc.pid"), "456").is_none()
        );
        assert!(
            parse_macos_air_mihomo_pid_file(Path::new("/tmp/air-mihomo-123.pid"), "abc").is_none()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_stale_pid_file_requires_dead_owner_and_matching_core_command() {
        let preview = CommandPreview {
            program: PathBuf::from("/Users/example/Library/Caches/org.air.Air/core/mihomo"),
            args: vec![
                "-d".into(),
                "/Users/example/Library/Caches/org.air.Air/core".into(),
                "-f".into(),
                "/Users/example/Library/Application Support/org.air.Air/core.runtime.config.yaml"
                    .into(),
            ],
            env: BTreeMap::new(),
            current_dir: PathBuf::from("/Users/example/Library/Caches/org.air.Air/core"),
            requires_admin: true,
        };
        let candidate = MacosAirMihomoPidFile {
            pid_file: PathBuf::from("/tmp/air-mihomo-100.pid"),
            owner_pid: 100,
            core_pid: 200,
        };
        let matching_argv = vec![
            preview.program.to_string_lossy().to_string(),
            "-d".into(),
            "/Users/example/Library/Caches/org.air.Air/core".into(),
            "-f".into(),
            "/Users/example/Library/Application Support/org.air.Air/core.runtime.config.yaml"
                .into(),
        ];

        assert!(macos_stale_pid_file_should_cleanup(
            &candidate,
            &preview,
            999,
            |pid| pid == 200,
            |pid| pid == 100,
            |pid| {
                assert_eq!(pid, 200);
                MacosCoreProcessMatch::Matches
            },
        ));
        assert!(!macos_stale_pid_file_should_cleanup(
            &candidate,
            &preview,
            999,
            |pid| pid == 200,
            |pid| pid == 100,
            |_| MacosCoreProcessMatch::DoesNotMatch,
        ));
        assert!(!macos_stale_pid_file_should_cleanup(
            &candidate,
            &preview,
            999,
            |pid| pid == 200,
            |pid| pid == 100,
            |_| MacosCoreProcessMatch::Unknown,
        ));
        assert!(!macos_stale_pid_file_should_cleanup(
            &candidate,
            &preview,
            999,
            |pid| pid == 200,
            |_| false,
            |_| MacosCoreProcessMatch::Matches,
        ));
        assert!(!macos_stale_pid_file_should_cleanup(
            &candidate,
            &preview,
            100,
            |pid| pid == 200,
            |pid| pid == 100,
            |_| MacosCoreProcessMatch::Matches,
        ));

        assert!(macos_process_argv_matches_preview(&matching_argv, &preview));
        let mut wrong_args = matching_argv;
        wrong_args[4] = "/tmp/other.yaml".into();
        assert!(!macos_process_argv_matches_preview(&wrong_args, &preview));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_existing_process_adoption_selects_matching_non_current_pid() {
        let preview = CommandPreview {
            program: PathBuf::from("/Users/example/Library/Caches/org.air.Air/core/mihomo"),
            args: vec![
                "-d".into(),
                "/Users/example/Library/Caches/org.air.Air/core".into(),
                "-f".into(),
                "/Users/example/Library/Application Support/org.air.Air/core.runtime.config.yaml"
                    .into(),
            ],
            env: BTreeMap::new(),
            current_dir: PathBuf::from("/Users/example/Library/Caches/org.air.Air/core"),
            requires_admin: true,
        };

        let selected =
            macos_select_adoptable_core_pid(&preview, 100, [100, 200, 300], |pid| match pid {
                200 => MacosCoreProcessMatch::DoesNotMatch,
                300 => MacosCoreProcessMatch::Matches,
                _ => MacosCoreProcessMatch::Unknown,
            });

        assert_eq!(selected, Some(300));
        assert_eq!(
            macos_select_adoptable_core_pid(&preview, 100, [100, 200], |_| {
                MacosCoreProcessMatch::Unknown
            }),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_ps_command_fallback_matches_exact_preview_command() {
        let preview = CommandPreview {
            program: PathBuf::from("/Users/example/Library/Caches/org.air.Air/core/mihomo"),
            args: vec![
                "-d".into(),
                "/Users/example/Library/Caches/org.air.Air/core".into(),
                "-f".into(),
                "/Users/example/Library/Application Support/org.air.Air/core.runtime.config.yaml"
                    .into(),
            ],
            env: BTreeMap::new(),
            current_dir: PathBuf::from("/Users/example/Library/Caches/org.air.Air/core"),
            requires_admin: true,
        };
        let command = "/Users/example/Library/Caches/org.air.Air/core/mihomo -d /Users/example/Library/Caches/org.air.Air/core -f /Users/example/Library/Application Support/org.air.Air/core.runtime.config.yaml";

        assert!(macos_process_command_matches_preview(command, &preview));
        assert!(!macos_process_command_matches_preview(
            "/Users/example/Library/Caches/org.air.Air/core/mihomo -d /tmp/other -f /tmp/other.yaml",
            &preview
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_procargs_parser_extracts_argv_after_exec_path_padding() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(3i32).to_ne_bytes());
        buffer.extend_from_slice(b"/usr/local/bin/mihomo\0\0\0");
        buffer.extend_from_slice(b"/usr/local/bin/mihomo\0-d\0/tmp/Air Data\0USER=ignored\0");

        let argv = parse_macos_procargs(&buffer).expect("procargs should parse");

        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/mihomo".to_string(),
                "-d".to_string(),
                "/tmp/Air Data".to_string(),
            ]
        );
    }
}
