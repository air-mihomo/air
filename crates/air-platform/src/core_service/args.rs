use air_error::{AppResult, PlatformError};

#[cfg(any(windows, test))]
use super::types::SERVICE_OWNER_PID_ARG;
use super::types::{CORE_SERVICE_ARG, CoreServicePaths};
pub fn service_binary_args(paths: &CoreServicePaths) -> AppResult<Vec<String>> {
    let exe = std::env::current_exe().map_err(|error| {
        PlatformError::OperationFailed(format!("读取当前程序路径失败: {error}"))
    })?;
    Ok(vec![
        exe.to_string_lossy().to_string(),
        CORE_SERVICE_ARG.to_string(),
        "--config-dir".to_string(),
        paths.config_dir.to_string_lossy().to_string(),
        "--data-dir".to_string(),
        paths.data_dir.to_string_lossy().to_string(),
        "--cache-dir".to_string(),
        paths.cache_dir.to_string_lossy().to_string(),
    ])
}

#[cfg(any(windows, test))]
pub(super) fn service_start_args(owner_pid: u32) -> Vec<String> {
    vec![SERVICE_OWNER_PID_ARG.to_string(), owner_pid.to_string()]
}
#[cfg(windows)]
pub(super) unsafe fn service_args_from_argv(argc: u32, argv: *mut *mut u16) -> Vec<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    if argv.is_null() {
        return Vec::new();
    }
    (0..argc)
        .filter_map(|index| {
            let ptr = unsafe { *argv.add(index as usize) };
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while unsafe { *ptr.add(len) } != 0 {
                len += 1;
            }
            let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
            Some(OsString::from_wide(slice).to_string_lossy().to_string())
        })
        .collect()
}

#[cfg(any(windows, test))]
pub(super) fn service_owner_pid_from_args(args: &[String]) -> Option<u32> {
    args.windows(2).find_map(|pair| {
        (pair[0] == SERVICE_OWNER_PID_ARG)
            .then(|| pair[1].parse::<u32>().ok())
            .flatten()
            .filter(|pid| *pid > 0)
    })
}
#[cfg(windows)]
pub(super) fn service_paths_from_args() -> AppResult<CoreServicePaths> {
    use std::path::PathBuf;

    let mut config_dir = None;
    let mut data_dir = None;
    let mut cache_dir = None;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config-dir" => config_dir = iter.next().map(PathBuf::from),
            "--data-dir" => data_dir = iter.next().map(PathBuf::from),
            "--cache-dir" => cache_dir = iter.next().map(PathBuf::from),
            _ => {}
        }
    }
    Ok(CoreServicePaths::from_base_dirs(
        &config_dir.ok_or_else(|| PlatformError::OperationFailed("服务缺少配置目录".into()))?,
        &data_dir.ok_or_else(|| PlatformError::OperationFailed("服务缺少数据目录".into()))?,
        &cache_dir.ok_or_else(|| PlatformError::OperationFailed("服务缺少缓存目录".into()))?,
    ))
}
#[cfg(windows)]
pub(super) fn wide_os_null(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub(super) fn wide_null(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
