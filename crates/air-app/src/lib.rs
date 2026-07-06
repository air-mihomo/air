extern crate self as air_app;

pub mod command;
pub mod events;
pub mod router;
pub mod runtime;
pub mod services;
pub mod state;
pub mod subscription_controller;
pub mod system_proxy_snapshot_store;

use air_error::AppResult;

pub use command::{AppCommand, CommandId, CommandResult};
pub use events::{AppEvent, AppNotificationLevel, AppSnapshot, RuntimeStatus};
pub use router::AppCommandRouter;
pub use runtime::{AppRuntime, BackgroundTask, CancellationToken};
pub use services::{AppServices, MihomoClientFactory};
pub use state::AppStateStore;
pub use subscription_controller::{SubscriptionController, SubscriptionStateProjection};

pub fn run_service_entrypoints() -> AppResult<bool> {
    air_telemetry::init_tracing();
    if air_platform::core_service::core_service_requested() {
        // Windows Service 与 GUI 共用同一二进制；服务入口必须在 GPUI 初始化前分流，避免后台服务创建桌面窗口。
        tracing::info!("air core service starting");
        air_platform::core_service::run_core_service_from_env()?;
        return Ok(true);
    }
    if air_platform::core_service::elevated_service_helper_requested() {
        // 服务安装/卸载是短生命周期提权 helper：GUI 保持普通权限，只等待本 helper 完成 SCM 操作。
        tracing::info!("air elevated service helper starting");
        air_platform::core_service::run_elevated_service_helper_from_env()?;
        return Ok(true);
    }
    if air_platform::elevated_process::elevated_core_helper_requested() {
        // 提权 helper 是隐藏的短生命周期进程，只负责启动 mihomo 并转储 stdout/stderr。
        // 这里必须在 GPUI 初始化前分流，避免 UAC 启动出的 helper 再创建一个主窗口。
        tracing::info!("air elevated core helper starting");
        air_platform::elevated_process::run_elevated_core_helper_from_env()?;
        return Ok(true);
    }
    Ok(false)
}

pub fn prepare_gui_launch() -> AppResult<
    Option<(
        bool,
        std::sync::mpsc::Receiver<air_platform::single_instance::SingleInstanceEvent>,
    )>,
> {
    tracing::info!("air application starting");
    // GUI 保持普通权限启动；需要 TUN 的场景只在核心进程启动时单独触发 UAC，
    // 避免应用自身重启导致窗口、托盘和页面状态被打断。
    let single_instance = match air_platform::single_instance::acquire_or_notify_existing()? {
        air_platform::single_instance::SingleInstance::Primary(server) => server,
        air_platform::single_instance::SingleInstance::AlreadyRunning => {
            tracing::info!("air GUI instance already running; requested existing window restore");
            return Ok(None);
        }
    };
    let force_start_core = air_platform::privilege::elevated_core_start_requested();
    Ok(Some((force_start_core, single_instance.into_receiver())))
}
