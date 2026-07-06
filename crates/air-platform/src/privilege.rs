use std::path::Path;

use air_error::{AppResult, PlatformError};

#[cfg(windows)]
use std::ffi::OsStr;

pub const ELEVATED_CORE_START_ARG: &str = "--air-start-core-after-elevation";
pub const ELEVATED_APP_START_ARG: &str = "--air-elevated-app-start";
pub const ELEVATED_CORE_HELPER_ARG: &str = "--air-elevated-core-helper";

pub fn elevated_core_start_requested() -> bool {
    std::env::args().any(|arg| arg == ELEVATED_CORE_START_ARG)
}

pub fn elevated_app_start_requested() -> bool {
    std::env::args().any(|arg| arg == ELEVATED_APP_START_ARG)
}

pub fn current_process_is_elevated() -> AppResult<bool> {
    current_process_is_elevated_impl()
}

pub fn relaunch_current_process_as_admin_for_app_start() -> AppResult<()> {
    relaunch_current_process_as_admin(false)
}

pub fn relaunch_current_process_as_admin_for_core_start() -> AppResult<()> {
    relaunch_current_process_as_admin(true)
}

fn relaunch_current_process_as_admin(start_core_after_elevation: bool) -> AppResult<()> {
    let exe = std::env::current_exe().map_err(|error| {
        PlatformError::OperationFailed(format!("读取当前程序路径失败: {error}"))
    })?;
    let current_dir = std::env::current_dir().ok();
    let args = elevated_relaunch_args(start_core_after_elevation);
    relaunch_as_admin(&exe, current_dir.as_deref(), &args)
}

fn elevated_relaunch_args(start_core_after_elevation: bool) -> Vec<String> {
    let mut args = std::env::args()
        .skip(1)
        .filter(|arg| arg != ELEVATED_CORE_START_ARG && arg != ELEVATED_APP_START_ARG)
        .collect::<Vec<_>>();
    args.push(ELEVATED_APP_START_ARG.to_string());
    if start_core_after_elevation {
        args.push(ELEVATED_CORE_START_ARG.to_string());
    }
    args
}

#[cfg(windows)]
fn current_process_is_elevated_impl() -> AppResult<bool> {
    use std::mem::{size_of, zeroed};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // Windows 的管理员状态来自进程 token。这里不猜测用户组，只读取 TokenElevation，避免本地化组名问题。
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(PlatformError::OperationFailed("读取进程权限 token 失败".into()).into());
    }

    let mut elevation: TOKEN_ELEVATION = unsafe { zeroed() };
    let mut returned_len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned_len,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(PlatformError::OperationFailed("读取进程提权状态失败".into()).into());
    }
    Ok(elevation.TokenIsElevated != 0)
}

#[cfg(unix)]
fn current_process_is_elevated_impl() -> AppResult<bool> {
    // macOS/Linux 的 TUN、路由和 DNS 劫持通常需要 root 权限；用 effective UID
    // 判断当前进程能力，避免普通用户启动时误以为已经具备权限。
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    Ok(unsafe { geteuid() } == 0)
}

#[cfg(not(any(windows, unix)))]
fn current_process_is_elevated_impl() -> AppResult<bool> {
    Ok(false)
}

#[cfg(windows)]
fn relaunch_as_admin(exe: &Path, current_dir: Option<&Path>, args: &[String]) -> AppResult<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let operation = wide_null("runas");
    let file = wide_os_null(exe.as_os_str());
    let parameters = wide_null(&join_windows_args(args));
    let directory = current_dir
        .map(|path| wide_os_null(path.as_os_str()))
        .unwrap_or_else(|| vec![0]);

    // ShellExecuteW + runas 是 Windows 桌面应用触发 UAC 的标准入口；真正的核心启动仍由提权后的应用实例完成。
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            directory.as_ptr(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        return Err(PlatformError::OperationFailed(format!(
            "管理员权限申请被取消或启动失败，ShellExecuteW={result}"
        ))
        .into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn relaunch_as_admin(_exe: &Path, _current_dir: Option<&Path>, _args: &[String]) -> AppResult<()> {
    Err(PlatformError::Unsupported("当前平台不支持 Windows UAC 提权启动".into()).into())
}

#[cfg(windows)]
fn wide_os_null(value: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(any(windows, test))]
pub(crate) fn join_windows_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(windows, test))]
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\'))
    {
        return arg.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    #[cfg(unix)]
    #[test]
    fn unix_elevated_check_matches_effective_uid() {
        let expected = unsafe { geteuid() } == 0;

        assert_eq!(
            current_process_is_elevated().unwrap(),
            expected,
            "Unix 平台必须根据有效 UID 判断是否已具备 TUN 所需的 root 权限"
        );
    }

    #[test]
    fn windows_args_are_quoted_for_elevated_relaunch() {
        assert_eq!(quote_windows_arg("plain"), "plain");
        assert_eq!(quote_windows_arg("has space"), "\"has space\"");
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(
            quote_windows_arg(r#"C:\Program Files\Air\"#),
            r#""C:\Program Files\Air\\""#
        );
    }
}
