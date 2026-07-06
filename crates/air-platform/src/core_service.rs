mod args;
#[cfg(windows)]
mod control;
#[cfg(windows)]
mod elevated;
#[cfg(windows)]
mod install;
#[cfg(not(windows))]
mod non_windows;
mod types;
#[cfg(windows)]
mod windows_acl;
#[cfg(windows)]
mod worker;

pub use args::service_binary_args;
#[cfg(windows)]
pub use control::{query_core_service, start_core_service, stop_core_service};
#[cfg(windows)]
pub use elevated::run_elevated_service_helper_from_env;
#[cfg(windows)]
pub use install::{install_core_service, uninstall_core_service};
#[cfg(not(windows))]
pub use non_windows::{
    install_core_service, query_core_service, run_core_service_from_env,
    run_elevated_service_helper_from_env, start_core_service, stop_core_service,
    uninstall_core_service,
};
pub use types::{
    CORE_SERVICE_DISPLAY_NAME, CORE_SERVICE_NAME, CoreServiceAction, CoreServicePaths,
    CoreServiceSnapshot,
};
#[cfg(windows)]
pub use worker::run_core_service_from_env;

pub fn core_service_requested() -> bool {
    std::env::args().any(|arg| arg == types::CORE_SERVICE_ARG)
}

#[cfg(windows)]
pub fn elevated_service_helper_requested() -> bool {
    std::env::args().any(|arg| arg == types::ELEVATED_SERVICE_HELPER_ARG)
}

#[cfg(not(windows))]
pub fn elevated_service_helper_requested() -> bool {
    false
}

pub fn core_service_required_for_admin_launch() -> bool {
    cfg!(target_os = "windows")
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use super::types::{CORE_SERVICE_ARG, SERVICE_OWNER_PID_ARG};

    #[test]
    fn service_binary_args_include_fixed_app_paths() {
        let paths = CoreServicePaths::from_base_dirs(
            Path::new(r"C:\Air\Config"),
            Path::new(r"C:\Air\Data"),
            Path::new(r"C:\Air\Cache"),
        );

        let args = service_binary_args(&paths).unwrap();

        assert!(args.iter().any(|arg| arg == CORE_SERVICE_ARG));
        assert!(args.iter().any(|arg| arg == r"C:\Air\Config"));
        assert!(args.iter().any(|arg| arg == r"C:\Air\Data"));
        assert!(args.iter().any(|arg| arg == r"C:\Air\Cache"));
    }

    #[test]
    fn service_start_args_include_owner_pid() {
        let args = args::service_start_args(42);

        assert_eq!(
            args::service_owner_pid_from_args(&args),
            Some(42),
            "服务启动参数需要携带 GUI owner pid，供服务在 GUI 被强杀后自停"
        );
    }

    #[test]
    fn service_owner_pid_ignores_missing_or_invalid_values() {
        assert_eq!(args::service_owner_pid_from_args(&[]), None);
        assert_eq!(
            args::service_owner_pid_from_args(&[SERVICE_OWNER_PID_ARG.into(), "abc".into()]),
            None
        );
        assert_eq!(
            args::service_owner_pid_from_args(&[SERVICE_OWNER_PID_ARG.into(), "0".into()]),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn service_security_sddl_keeps_admin_delete_and_write_dac() {
        let sddl = windows_acl::core_service_security_sddl(None);

        assert!(sddl.contains(";;;BA)"));
        assert!(
            sddl.contains("SDRCWDWO"),
            "Administrators 需要保留 DELETE/READ_CONTROL/WRITE_DAC/WRITE_OWNER，确保旧服务可维护和可卸载"
        );
        assert!(
            sddl.contains("(A;;LCRPWP;;;BU)"),
            "普通 Users 只应具备查询、启动和停止服务的最小交互权限"
        );
    }
}
