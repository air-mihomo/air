use air_app::RuntimeStatus;
use air_app::events::AppNotificationLevel;
use air_error::{AppResult, RuntimeError};

use super::context::CommandExecutionContext;
use super::shared::{apply_current_mihomo_status, ensure_not_canceled, runtime_is_running};

pub(super) async fn handle_detect_core(context: &CommandExecutionContext) -> AppResult<()> {
    tracing::info!("starting core detection command");
    ensure_not_canceled(&context.token)?;
    context.services.detect_core().await?;
    tracing::info!("core detection command completed");
    Ok(())
}

pub(super) async fn handle_prepare_core(context: &CommandExecutionContext) -> AppResult<()> {
    tracing::info!("starting core prepare command");
    ensure_not_canceled(&context.token)?;
    context.services.prepare_core().await?;
    tracing::info!("core prepare command completed");
    Ok(())
}

pub(super) async fn handle_start_core(context: &CommandExecutionContext) -> AppResult<()> {
    tracing::info!("starting core launch command");
    ensure_not_canceled(&context.token)?;
    let config = context.services.launch_config().await?;
    tracing::info!(
        binary = %config.binary_path.display(),
        config = %config.config_path.display(),
        working_dir = %config.working_dir.display(),
        requires_admin = config.requires_admin,
        "core launch config resolved"
    );
    if core_service_missing_for_admin_launch(context, config.requires_admin)? {
        context.services.emit_notification(
            AppNotificationLevel::Warning,
            "TUN 需要先安装内核服务；请在设置页打开“内核服务”。",
        );
        context.cancellations.remove("core");
        return Err(RuntimeError::UnsupportedCommand(
            "内核服务未安装，无法启动需要管理员权限的 TUN 内核".into(),
        )
        .into());
    }
    context
        .services
        .snapshots
        .set_runtime_status(RuntimeStatus::Starting);
    let status = match context.services.mihomo.start(config).await {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(error = %error, "core launch command failed");
            apply_current_mihomo_status(&context.services).await;
            context.cancellations.remove("core");
            return Err(error);
        }
    };
    tracing::info!(phase = ?status.phase, process = ?status.process, "core launch command completed");
    context.services.apply_mihomo_status(status);
    context.services.sync_system_proxy_to_current_config()?;
    context.cancellations.remove("core");
    Ok(())
}

pub(super) async fn handle_stop_core(context: &CommandExecutionContext) -> AppResult<()> {
    tracing::info!("starting core stop command");
    ensure_not_canceled(&context.token)?;
    context
        .services
        .snapshots
        .set_runtime_status(RuntimeStatus::Stopping);
    let status = match context.services.mihomo.stop().await {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(error = %error, "core stop command failed");
            apply_current_mihomo_status(&context.services).await;
            context.cancellations.remove("core");
            return Err(error);
        }
    };
    tracing::info!(phase = ?status.phase, process = ?status.process, "core stop command completed");
    context.services.apply_mihomo_status(status);
    context.services.disable_system_proxy();
    context.cancellations.remove("core");
    Ok(())
}

pub(super) async fn handle_restart_core(context: &CommandExecutionContext) -> AppResult<()> {
    ensure_not_canceled(&context.token)?;
    if runtime_is_running(&context.services) {
        if context.services.current_config_enables_tun()? {
            tracing::info!("restart requires admin; using managed full restart");
        } else {
            // 运行中重启优先走 mihomo 的快速 /restart；Windows 服务托管路径会在旧子进程退出后
            // 保持服务和 JobObject 存活，让 mihomo 自身的后台重启不会被托管边界误杀。
            context.services.write_runtime_config_validated().await?;
            context
                .services
                .mihomo_clients
                .client()?
                .restart_core_default()
                .await?;
            context
                .services
                .emit_notification(AppNotificationLevel::Success, "内核重启请求已发送");
            context.cancellations.remove("core");
            return Ok(());
        }
    }
    let config = context.services.launch_config().await?;
    if core_service_missing_for_admin_launch(context, config.requires_admin)? {
        context.services.emit_notification(
            AppNotificationLevel::Warning,
            "TUN 需要先安装内核服务；请在设置页打开“内核服务”。",
        );
        context.cancellations.remove("core");
        return Err(RuntimeError::UnsupportedCommand(
            "内核服务未安装，无法重启需要管理员权限的 TUN 内核".into(),
        )
        .into());
    }
    context
        .services
        .snapshots
        .set_runtime_status(RuntimeStatus::Starting);
    let status = match context.services.mihomo.restart(config).await {
        Ok(status) => status,
        Err(error) => {
            apply_current_mihomo_status(&context.services).await;
            context.cancellations.remove("core");
            return Err(error);
        }
    };
    context.services.apply_mihomo_status(status);
    context.services.sync_system_proxy_to_current_config()?;
    context.cancellations.remove("core");
    Ok(())
}

pub(super) async fn handle_refresh_core_service(
    context: &CommandExecutionContext,
) -> AppResult<()> {
    ensure_not_canceled(&context.token)?;
    context.services.refresh_core_service_projection()?;
    Ok(())
}

pub(super) async fn handle_install_core_service(
    context: &CommandExecutionContext,
) -> AppResult<()> {
    ensure_not_canceled(&context.token)?;
    let snapshot = context.services.install_core_service()?;
    context.services.emit_notification(
        AppNotificationLevel::Success,
        if snapshot.installed {
            "内核服务已安装"
        } else {
            "内核服务安装状态未确认"
        },
    );
    Ok(())
}

pub(super) async fn handle_uninstall_core_service(
    context: &CommandExecutionContext,
) -> AppResult<()> {
    ensure_not_canceled(&context.token)?;
    let snapshot = context.services.uninstall_core_service()?;
    context.services.emit_notification(
        AppNotificationLevel::Success,
        if snapshot.installed {
            "内核服务仍处于已安装状态"
        } else {
            "内核服务已卸载"
        },
    );
    Ok(())
}

fn core_service_missing_for_admin_launch(
    context: &CommandExecutionContext,
    requires_admin: bool,
) -> AppResult<bool> {
    Ok(requires_admin
        && air_platform::core_service::core_service_required_for_admin_launch()
        && !context
            .services
            .refresh_core_service_projection()?
            .installed)
}
