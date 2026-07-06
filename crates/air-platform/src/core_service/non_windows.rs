use air_error::{AppResult, PlatformError};

use super::types::{CoreServicePaths, CoreServiceSnapshot};

pub fn query_core_service() -> AppResult<CoreServiceSnapshot> {
    Ok(CoreServiceSnapshot::default())
}

pub fn start_core_service() -> AppResult<()> {
    unsupported_windows_service()
}

pub fn stop_core_service() -> AppResult<()> {
    unsupported_windows_service()
}

pub fn install_core_service(_paths: &CoreServicePaths) -> AppResult<CoreServiceSnapshot> {
    unsupported_windows_service_elevation()
}

pub fn uninstall_core_service() -> AppResult<CoreServiceSnapshot> {
    unsupported_windows_service_elevation()
}

pub fn run_core_service_from_env() -> AppResult<()> {
    unsupported_windows_service()
}

pub fn run_elevated_service_helper_from_env() -> AppResult<()> {
    unsupported_windows_service_elevation()
}

pub(super) fn unsupported_windows_service<T>() -> AppResult<T> {
    Err(PlatformError::Unsupported("当前平台不支持 Windows 服务".into()).into())
}

pub(super) fn unsupported_windows_service_elevation<T>() -> AppResult<T> {
    Err(PlatformError::Unsupported("当前平台不支持 Windows 服务提权操作".into()).into())
}
