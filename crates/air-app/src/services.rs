use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use air_app::{
    AppEvent, AppNotificationLevel, AppRuntime, AppSnapshot, AppStateStore, RuntimeStatus,
};
use air_app::{SubscriptionController, SubscriptionStateProjection};
use air_config::{
    ConfigDocument, MihomoConfigDocument, SubscriptionMergeInput, apply_override_script,
};
use air_error::{AppError, AppResult, ConfigError, ProcessError};
use air_mihomo::service::MihomoServicePhase;
#[cfg(not(test))]
use air_mihomo::{MihomoConfigTestOptions, RuntimeDetector, test_mihomo_config};
use air_mihomo::{MihomoEndpoint, MihomoHttpClient, MihomoStreamClient};
use air_mihomo::{
    MihomoProcessManager, MihomoRuntimeDetector, MihomoService, MihomoServiceStatus,
    ProcessLaunchConfig, RuntimeDetectionOptions,
};
use air_platform::core_service::{self, CoreServicePaths, CoreServiceSnapshot};
use air_settings::AppSettings;
use air_storage::{
    AppPaths, CoreConfigStore, OverrideScriptStore, SettingsStore, SubscriptionStore,
};
use air_telemetry::redaction::redact_log_value;

use crate::system_proxy_snapshot_store::SystemProxySnapshotStore;

pub type AppMihomoService =
    MihomoService<MihomoRuntimeDetector, MihomoProcessManager, MihomoHttpClient>;

#[derive(Clone)]
pub struct AppServices {
    pub runtime: Arc<AppRuntime>,
    pub paths: AppPaths,
    pub settings_store: Arc<SettingsStore>,
    pub core_config_store: Arc<CoreConfigStore>,
    pub subscription_store: Arc<SubscriptionStore>,
    pub override_script_store: Arc<OverrideScriptStore>,
    pub system_proxy_snapshots: Arc<SystemProxySnapshotStore>,
    pub mihomo: Arc<AppMihomoService>,
    pub mihomo_clients: MihomoClientFactory,
    pub snapshots: AppStateStore,
    shutdown_stop_started: Arc<AtomicBool>,
}

impl AppServices {
    pub fn new() -> AppResult<Self> {
        let paths = AppPaths::resolve()?;
        Self::with_paths(paths)
    }

    pub fn with_paths(paths: AppPaths) -> AppResult<Self> {
        paths.init()?;
        tracing::info!(
            config_dir = %paths.config_dir.display(),
            data_dir = %paths.data_dir.display(),
            cache_dir = %paths.cache_dir.display(),
            "initializing app services"
        );
        #[cfg(not(test))]
        if let Some(path) = air_mihomo::embedded::ensure_embedded_core_installed(&paths.cores_dir)?
        {
            tracing::info!(path = %path.display(), "installed embedded mihomo core");
        }
        #[cfg(not(test))]
        for path in air_mihomo::embedded::ensure_embedded_geodata_installed(&paths.cores_dir)? {
            tracing::info!(path = %path.display(), "installed embedded mihomo geodata");
        }
        let runtime = Arc::new(AppRuntime::new()?);
        let settings_store = Arc::new(SettingsStore::new(paths.clone()));
        let _settings = settings_store.ensure_exists()?;
        let core_config_store = Arc::new(CoreConfigStore::new(paths.clone()));
        let core_document = Some(core_config_store.ensure_user_config_exists()?);
        let endpoint = MihomoClientFactory::endpoint_from_document(core_document.as_ref());
        let health_client = Arc::new(MihomoHttpClient::new(endpoint));
        let process = Arc::new(MihomoProcessManager::new());
        let detector = Arc::new(MihomoRuntimeDetector);
        let mut initial_snapshot = AppSnapshot {
            active_profile: Some("core.common.config.yaml".to_string()),
            controller_addr: Some(
                MihomoClientFactory::endpoint_from_document(core_document.as_ref()).base_url,
            ),
            ..AppSnapshot::default()
        };
        initial_snapshot.core_service = core_service::query_core_service().unwrap_or_default();
        tracing::info!(
            controller_addr = initial_snapshot
                .controller_addr
                .as_deref()
                .unwrap_or_default(),
            core_service_installed = initial_snapshot.core_service.installed,
            core_service_running = initial_snapshot.core_service.running,
            "app services initialized"
        );

        Ok(Self {
            runtime: Arc::clone(&runtime),
            paths: paths.clone(),
            settings_store: Arc::clone(&settings_store),
            core_config_store: Arc::clone(&core_config_store),
            subscription_store: Arc::new(SubscriptionStore::new(paths.clone())),
            override_script_store: Arc::new(OverrideScriptStore::new(paths.clone())),
            system_proxy_snapshots: Arc::new(SystemProxySnapshotStore::new(paths)),
            mihomo: Arc::new(MihomoService::new(detector, process, health_client)),
            mihomo_clients: MihomoClientFactory::new(Arc::clone(&core_config_store)),
            // AppSnapshot 是 UI 的只读投影；真实业务状态仍由 app/service/domain 持有。
            // 所有投影字段统一经 AppStateStore 写入，避免页面或服务各自散落修改。
            snapshots: AppStateStore::new(Arc::clone(&runtime), initial_snapshot),
            shutdown_stop_started: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn load_settings(&self) -> AppResult<AppSettings> {
        tracing::info!("loading app settings from settings store");
        self.settings_store.load()
    }

    pub fn save_settings(&self, settings: &AppSettings) -> AppResult<()> {
        tracing::info!("saving app settings through app services");
        self.settings_store.save(settings)?;
        self.refresh_settings_projection();
        Ok(())
    }

    pub fn detection_options(&self) -> AppResult<RuntimeDetectionOptions> {
        // 核心启动前 external-controller 本来可能尚未监听；只在已确认运行时做 controller 探测，
        // 避免应用启动自动准备阶段留下误导性的“controller 不可达”诊断。
        let controller_addr = if matches!(self.snapshots.snapshot().runtime, RuntimeStatus::Running)
        {
            Some(self.current_endpoint().base_url)
        } else {
            None
        };
        Ok(RuntimeDetectionOptions::new(
            Some(self.paths.config_dir.clone()),
            self.paths.cores_dir.clone(),
            controller_addr,
        ))
    }

    pub async fn detect_core(&self) -> AppResult<()> {
        tracing::info!("detecting mihomo runtime");
        let status = self.mihomo.prepare(self.detection_options()?).await?;
        self.apply_mihomo_status(status);
        Ok(())
    }

    pub async fn prepare_core(&self) -> AppResult<()> {
        tracing::info!("preparing mihomo runtime");
        let status = self.mihomo.prepare(self.detection_options()?).await?;
        self.apply_mihomo_status(status);
        Ok(())
    }

    pub async fn launch_config(&self) -> AppResult<ProcessLaunchConfig> {
        let requires_admin = self.current_config_enables_tun()?;
        tracing::info!(requires_admin, "building mihomo launch config");
        let status = self.mihomo.prepare(self.detection_options()?).await?;
        let binary_path = status
            .runtime
            .and_then(|runtime| runtime.binary_path)
            .ok_or_else(|| ProcessError::InvalidState("未找到可启动的 mihomo 核心".into()))?;
        let working_dir = self.paths.cores_dir.clone();
        let config_path = self
            .write_runtime_config_with_binary(binary_path.clone())
            .await?;
        tracing::info!(
            binary = %binary_path.display(),
            config = %config_path.display(),
            working_dir = %working_dir.display(),
            requires_admin,
            "mihomo launch config built"
        );

        // mihomo 的 -d 目录固定放在缓存目录的 core 子目录，核心生成的辅助文件不混入用户配置目录；
        // 运行配置仍通过 -f 指向 config 目录，二者分离可以降低配置保存和核心运行态文件互相覆盖的风险。
        Ok(ProcessLaunchConfig {
            binary_path,
            config_path,
            working_dir,
            console_log_path: Some(self.paths.logs_dir.join("core.log")),
            env: BTreeMap::new(),
            safe_paths: vec![
                self.paths.config_dir.clone(),
                self.paths.data_dir.clone(),
                self.paths.cache_dir.clone(),
            ],
            requires_admin,
        })
    }

    pub fn import_profile_file(&self, path: &Path) -> AppResult<()> {
        tracing::info!(path = %path.display(), "importing core profile without mihomo validation");
        let source = std::fs::read_to_string(path).map_err(air_error::StorageError::Io)?;
        let document = ConfigDocument::parse(source)?;
        self.core_config_store.save_user_config(&document)?;
        self.apply_core_config_projection();
        Ok(())
    }

    pub async fn import_profile_file_validated(&self, path: &Path) -> AppResult<()> {
        tracing::info!(path = %path.display(), "importing core profile with mihomo validation");
        let source = std::fs::read_to_string(path).map_err(air_error::StorageError::Io)?;
        let document = ConfigDocument::parse(source)?;
        self.validate_document_with_mihomo(&document, "core-common-config-import")
            .await?;
        self.core_config_store.save_user_config(&document)?;
        self.apply_core_config_projection();
        Ok(())
    }

    pub fn save_current_config(&self, source: &str) -> AppResult<()> {
        tracing::info!(
            bytes = source.len(),
            "saving current core config without mihomo validation"
        );
        let document = ConfigDocument::parse(source.to_string())?;
        self.core_config_store.save_user_config(&document)?;
        self.apply_core_config_projection();
        Ok(())
    }

    pub async fn save_current_config_validated(&self, source: &str) -> AppResult<()> {
        tracing::info!(
            bytes = source.len(),
            "saving current core config with mihomo validation"
        );
        let document = ConfigDocument::parse(source.to_string())?;
        self.validate_document_with_mihomo(&document, "core-common-config-save")
            .await?;
        self.core_config_store.save_user_config(&document)?;
        self.apply_core_config_projection();
        Ok(())
    }

    pub fn save_runtime_mode(&self, mode: &str) -> AppResult<()> {
        tracing::info!(mode, "saving runtime mode");
        let mut document = self.core_config_store.load_user_config()?;
        // 状态栏运行模式是全局配置里的单字段热更新；这里只改 mode，避免把运行态 PATCH 当成整份配置保存。
        document.typed.global.mode = Some(mode.to_string());
        self.core_config_store.save_user_config(&document)?;
        self.apply_core_config_projection();
        Ok(())
    }

    pub async fn save_runtime_mode_validated(&self, mode: &str) -> AppResult<()> {
        tracing::info!(mode, "saving runtime mode with mihomo validation");
        let mut document = self.core_config_store.load_user_config()?;
        // 运行模式虽然是单字段修改，但仍会落盘到用户配置；写入前使用 mihomo 本体确认整份 YAML 可加载。
        document.typed.global.mode = Some(mode.to_string());
        self.validate_document_with_mihomo(&document, "core-common-config-mode")
            .await?;
        self.core_config_store.save_user_config(&document)?;
        self.apply_core_config_projection();
        Ok(())
    }

    pub fn current_profile_document(&self) -> AppResult<Option<ConfigDocument>> {
        Ok(Some(self.core_config_store.load_user_config()?))
    }

    pub fn runtime_or_current_profile_document(&self) -> AppResult<Option<ConfigDocument>> {
        let runtime_path = self.core_config_store.runtime_config_path();
        if runtime_path.is_file() {
            let source =
                std::fs::read_to_string(&runtime_path).map_err(air_error::StorageError::Io)?;
            // 核心运行时实际使用的是合并后的 runtime 配置；代理组运行态成员可能来自订阅，
            // 因此刷新投影时优先读取这份文件，才能正确解析订阅节点的协议类型。
            return Ok(Some(ConfigDocument::parse(source)?));
        }
        self.current_profile_document()
    }

    pub fn selected_subscription_document(&self) -> AppResult<Option<ConfigDocument>> {
        let sources = self.subscription_store.load_sources()?;
        let Some(source) = sources.iter().find(|source| source.enabled) else {
            return Ok(None);
        };
        let Some(bytes) = self.subscription_store.read_cached_content(&source.id)? else {
            return Ok(None);
        };
        let source_text = String::from_utf8(bytes).map_err(|error| {
            ConfigError::Subscription(format!("订阅缓存不是有效 UTF-8 YAML: {error}"))
        })?;

        // 这是代理组页面使用的只读订阅投影；只有 enabled 订阅才代表当前激活来源。
        // 解析订阅缓存不会修改运行配置，
        // 真正参与 mihomo 启动的合并仍由 subscription_merge_inputs/write_runtime_config 负责。
        Ok(Some(ConfigDocument::parse(source_text)?))
    }

    pub fn refresh_settings_projection(&self) {
        let endpoint = self.current_endpoint();
        tracing::info!(controller_addr = %endpoint.base_url, "refreshing settings projection");
        self.snapshots.set_controller_addr(Some(endpoint.base_url));
    }

    pub fn emit_notification(&self, level: AppNotificationLevel, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(level = ?level, message = %redact_log_value(&message), "emitting user notification");
        self.runtime.emit(AppEvent::UserNotification {
            level,
            message: redact_log_value(&message),
        });
    }

    pub fn subscription_controller(&self) -> SubscriptionController {
        SubscriptionController::new((*self.subscription_store).clone())
    }

    pub fn subscription_projection(&self) -> AppResult<SubscriptionStateProjection> {
        self.subscription_controller().load_projection()
    }

    pub fn emit_subscription_projection(&self) -> AppResult<()> {
        tracing::info!("emitting subscription projection");
        self.runtime.emit(AppEvent::SubscriptionStateChanged(
            self.subscription_projection()?,
        ));
        Ok(())
    }

    pub fn stop_core_before_exit(&self) -> AppResult<()> {
        if self.shutdown_stop_started.swap(true, Ordering::AcqRel) {
            tracing::info!(
                "mihomo core shutdown stop already requested; skipping duplicate app-exit stop"
            );
            return Ok(());
        }
        tracing::info!("stopping mihomo core before app exit");
        // 退出阶段不能依赖异步命令路由继续调度；这里直接复用应用持有的 Tokio runtime，
        // 等待 MihomoService 完成 stop，确保受管子进程不会在 GUI 退出后残留。
        let mut first_error: Option<AppError> = None;
        let status = match self.runtime.block_on(self.mihomo.stop()) {
            Ok(status) => Some(status),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "managed mihomo stop failed during app exit; continuing service fallback"
                );
                first_error = Some(error);
                None
            }
        };
        // 旧版本或外部启动可能让 Windows 服务处于运行中但不在当前进程管理器的 child 槽位；
        // 正常退出时再按服务状态补一次停止，和 owner-pid 自停机制一起覆盖普通退出与强杀两条路径。
        #[cfg(all(not(test), target_os = "windows"))]
        {
            match core_service::query_core_service() {
                Ok(snapshot) if snapshot.running => match core_service::stop_core_service() {
                    Ok(()) => {
                        // 服务补停成功代表退出收尾已经达成目标；前面的 1061/过渡态错误只作为诊断日志保留。
                        first_error = None;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "core service fallback stop failed during app exit"
                        );
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                },
                Ok(_) => {
                    // 服务最终不在运行态时，关闭流程不再被前序瞬时错误阻塞。
                    first_error = None;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "failed to query core service during app exit fallback"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(status) = status {
            self.apply_mihomo_status(status);
        } else {
            // 退出时如果托管 stop 已经失败，但服务兜底确认没有运行或已尝试停止，
            // UI 投影仍收敛到空闲，避免关闭流程继续表现为“停止中”。
            self.snapshots.set_runtime_status(RuntimeStatus::Idle);
        }
        if let Some(error) = first_error {
            self.shutdown_stop_started.store(false, Ordering::Release);
            return Err(error);
        }
        self.disable_system_proxy();
        Ok(())
    }

    pub fn refresh_core_service_projection(&self) -> AppResult<CoreServiceSnapshot> {
        let snapshot = core_service::query_core_service()?;
        tracing::info!(
            installed = snapshot.installed,
            running = snapshot.running,
            "refreshed core service projection"
        );
        self.snapshots.set_core_service(snapshot);
        Ok(snapshot)
    }

    pub fn install_core_service(&self) -> AppResult<CoreServiceSnapshot> {
        tracing::info!("installing core service");
        let snapshot = core_service::install_core_service(&core_service_paths(&self.paths))?;
        self.snapshots.set_core_service(snapshot);
        Ok(snapshot)
    }

    pub fn uninstall_core_service(&self) -> AppResult<CoreServiceSnapshot> {
        tracing::info!("uninstalling core service");
        let snapshot = core_service::uninstall_core_service()?;
        self.snapshots.set_core_service(snapshot);
        Ok(snapshot)
    }

    pub fn apply_mihomo_status(&self, status: MihomoServiceStatus) {
        tracing::info!(
            phase = ?status.phase,
            process = ?status.process,
            runtime_present = status.runtime.is_some(),
            diagnostics = status.diagnostics.len(),
            last_error = status.last_error.as_deref().map(redact_log_value),
            "applying mihomo status to snapshot"
        );
        let runtime_status = runtime_status_from_mihomo(&status);
        self.snapshots
            .set_runtime_projection(runtime_status, status.runtime, status.last_error);
    }

    fn apply_core_config_projection(&self) {
        // 单配置模式下 active_profile 只作为 UI 顶部的来源标签，不再代表可切换 profile。
        self.snapshots
            .set_active_profile(Some("core.common.config.yaml".to_string()));
        self.refresh_settings_projection();
    }

    pub fn write_runtime_config(&self) -> AppResult<PathBuf> {
        tracing::info!("writing runtime config without mihomo binary validation");
        let runtime = self.build_effective_runtime_config(None)?;
        self.core_config_store.write_runtime_document(&runtime)
    }

    pub async fn write_runtime_config_validated(&self) -> AppResult<PathBuf> {
        tracing::info!("writing runtime config with mihomo binary validation");
        let binary_path = self.resolve_mihomo_binary_for_config_test().await?;
        self.write_runtime_config_with_binary(binary_path).await
    }

    async fn write_runtime_config_with_binary(&self, binary_path: PathBuf) -> AppResult<PathBuf> {
        tracing::info!(binary = %binary_path.display(), "building validated runtime config");
        let runtime = self.build_effective_runtime_config(None)?;
        let yaml = CoreConfigStore::runtime_config_yaml(&runtime)?;
        self.test_config_yaml_with_binary(binary_path, &yaml, "core-runtime-config-write")
            .await?;
        self.core_config_store.write_runtime_document(&runtime)
    }

    pub fn preview_override_script(&self, script: &str) -> AppResult<String> {
        tracing::info!(bytes = script.len(), "previewing override script");
        let runtime = self.build_effective_runtime_config(Some(script))?;
        let runtime_yaml = CoreConfigStore::runtime_config_yaml(&runtime)?;
        Ok(runtime_yaml)
    }

    pub fn load_override_script(&self) -> AppResult<String> {
        tracing::info!("loading override script");
        self.override_script_store.load_or_default()
    }

    pub fn save_override_script(&self, script: &str, enabled: bool) -> AppResult<()> {
        tracing::info!(bytes = script.len(), enabled, "saving override script");
        self.override_script_store.save(script)?;
        self.set_override_script_enabled(enabled)
    }

    pub fn set_override_script_enabled(&self, enabled: bool) -> AppResult<()> {
        tracing::info!(enabled, "updating override script enabled flag");
        let mut settings = self.settings_store.load()?;
        settings.override_script_enabled = enabled;
        self.settings_store.save(&settings)?;
        Ok(())
    }

    fn build_effective_runtime_config(
        &self,
        debug_script: Option<&str>,
    ) -> AppResult<MihomoConfigDocument> {
        tracing::info!(
            debug_script = debug_script.is_some(),
            "building effective runtime config"
        );
        let mut document = self.core_config_store.load_user_config()?;
        ensure_runtime_dns_defaults(&mut document);
        let subscriptions = self.subscription_merge_inputs()?;
        let subscription_name = active_subscription_name(&subscriptions);
        let runtime =
            CoreConfigStore::merged_runtime_config(&document.typed, subscriptions.as_slice());

        let Some(script) = debug_script else {
            let settings = self.settings_store.load()?;
            if !settings.override_script_enabled {
                return Ok(runtime);
            }
            let script = self.override_script_store.load_or_default()?;
            return Ok(apply_override_script(
                &subscription_name,
                &runtime,
                &script,
            )?);
        };

        Ok(apply_override_script(&subscription_name, &runtime, script)?)
    }

    pub fn current_config_enables_tun(&self) -> AppResult<bool> {
        Ok(document_enables_tun(
            &self.core_config_store.load_user_config()?,
        ))
    }

    pub fn current_mixed_port(&self) -> AppResult<Option<u32>> {
        Ok(self.build_effective_runtime_config(None)?.global.mixed_port)
    }

    pub fn sync_system_proxy_to_current_config(&self) -> AppResult<()> {
        if !self.load_settings()?.system_proxy_enabled {
            tracing::info!("restoring system proxy because system proxy sync is disabled");
            self.disable_system_proxy();
            return Ok(());
        }

        #[cfg(not(target_os = "macos"))]
        {
            tracing::info!("skipping system proxy sync because current platform is unsupported");
            return Ok(());
        }

        let Some(port) = self.current_mixed_port()? else {
            tracing::info!("restoring system proxy because mixed-port is not configured");
            self.disable_system_proxy();
            return Ok(());
        };
        let snapshot = match self.ensure_system_proxy_snapshot(port) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, port, "failed to capture system proxy snapshot");
                self.emit_notification(
                    AppNotificationLevel::Warning,
                    format!("系统代理原始状态保存失败：{error}"),
                );
                return Ok(());
            }
        };
        match enable_local_system_proxy_for_runtime(port) {
            Ok(update) => {
                if snapshot.managed_port != port {
                    let mut confirmed_snapshot = snapshot.clone();
                    confirmed_snapshot.managed_port = port;
                    if let Err(error) = self.system_proxy_snapshots.save(&confirmed_snapshot) {
                        tracing::warn!(%error, port, "failed to confirm system proxy managed port");
                        self.emit_notification(
                            AppNotificationLevel::Warning,
                            format!("系统代理托管状态保存失败：{error}"),
                        );
                        self.restore_system_proxy_after_failed_enable(&snapshot, port);
                        return Ok(());
                    }
                }
                tracing::info!(
                    port,
                    services = ?update.services,
                    "system proxy synchronized to mihomo mixed-port"
                );
            }
            Err(error) => {
                tracing::warn!(%error, port, "failed to synchronize system proxy");
                self.emit_notification(
                    AppNotificationLevel::Warning,
                    format!("系统代理同步失败：{error}"),
                );
                self.restore_system_proxy_after_failed_enable(&snapshot, port);
            }
        }
        Ok(())
    }

    fn ensure_system_proxy_snapshot(
        &self,
        port: u32,
    ) -> AppResult<air_platform::system_proxy::SystemProxySnapshot> {
        if let Some(snapshot) = self.system_proxy_snapshots.load()? {
            tracing::debug!(
                managed_port = snapshot.managed_port,
                port,
                "system proxy snapshot already exists"
            );
            return Ok(snapshot);
        }

        let snapshot = capture_system_proxy_snapshot_for_runtime(port)?;
        self.system_proxy_snapshots.save(&snapshot)?;
        tracing::info!(
            port,
            services = ?snapshot
                .services
                .iter()
                .map(|service| service.service.as_str())
                .collect::<Vec<_>>(),
            "captured system proxy snapshot"
        );
        Ok(snapshot)
    }

    fn restore_system_proxy_after_failed_enable(
        &self,
        snapshot: &air_platform::system_proxy::SystemProxySnapshot,
        attempted_port: u32,
    ) {
        let mut restore_failed = false;
        let mut skipped_services = Vec::new();
        if snapshot.managed_port != attempted_port {
            let mut attempted_snapshot = snapshot.clone();
            attempted_snapshot.managed_port = attempted_port;
            match restore_system_proxy_snapshot_for_runtime(&attempted_snapshot) {
                Ok(update) => skipped_services.extend(update.skipped_services),
                Err(error) => {
                    restore_failed = true;
                    tracing::warn!(
                        %error,
                        attempted_port,
                        "failed to restore partially updated system proxy after sync failure"
                    );
                }
            }
        }

        match restore_system_proxy_snapshot_for_runtime(snapshot) {
            Ok(update) => skipped_services.extend(update.skipped_services),
            Err(error) => {
                restore_failed = true;
                tracing::warn!(%error, "failed to restore system proxy after sync failure");
            }
        }

        if restore_failed || !skipped_services.is_empty() {
            tracing::warn!(
                skipped_services = ?skipped_services,
                "kept system proxy snapshot because failed sync was not fully restored"
            );
            return;
        }

        if let Err(error) = self.system_proxy_snapshots.delete() {
            tracing::warn!(%error, "failed to delete system proxy snapshot after failed sync restore");
        }
    }

    pub fn disable_system_proxy(&self) {
        let snapshot = match self.system_proxy_snapshots.load() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "failed to load system proxy snapshot before restore");
                return;
            }
        };
        if let Some(snapshot) = snapshot {
            match restore_system_proxy_snapshot_for_runtime(&snapshot) {
                Ok(update) => {
                    tracing::info!(
                        restored_services = ?update.restored_services,
                        skipped_services = ?update.skipped_services,
                        "system proxy snapshot restored after mihomo stopped"
                    );
                    if update.skipped_services.is_empty() {
                        if let Err(error) = self.system_proxy_snapshots.delete() {
                            tracing::warn!(%error, "failed to delete restored system proxy snapshot");
                        }
                    } else {
                        tracing::warn!(
                            skipped_services = ?update.skipped_services,
                            "kept system proxy snapshot because some services were not restored"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to restore system proxy snapshot");
                }
            }
        }
    }

    fn subscription_merge_inputs(&self) -> AppResult<Vec<SubscriptionMergeInput>> {
        let mut inputs = Vec::new();
        for source in self.subscription_store.load_sources()? {
            if !source.enabled {
                continue;
            }
            tracing::info!(
                subscription_id = %source.id,
                subscription_name = %source.name,
                "loading enabled subscription for runtime merge"
            );
            let Some(bytes) = self.subscription_store.read_cached_content(&source.id)? else {
                tracing::warn!(subscription_id = %source.id, "enabled subscription cache missing during runtime merge");
                continue;
            };
            let source_text = String::from_utf8(bytes).map_err(|error| {
                ConfigError::Subscription(format!("订阅缓存不是有效 UTF-8 YAML: {error}"))
            })?;
            let document = ConfigDocument::parse(source_text)?;
            inputs.push(SubscriptionMergeInput {
                id: source.id,
                display_name: source.name,
                enabled: true,
                document: document.typed,
            });
        }
        tracing::info!(count = inputs.len(), "prepared subscription merge inputs");
        Ok(inputs)
    }

    fn current_endpoint(&self) -> MihomoEndpoint {
        let document = self.core_config_store.load_user_config().ok();
        MihomoClientFactory::endpoint_from_document(document.as_ref())
    }

    async fn validate_document_with_mihomo(
        &self,
        document: &ConfigDocument,
        scope: &str,
    ) -> AppResult<()> {
        tracing::info!(
            scope,
            proxies = document.typed.proxies.len(),
            proxy_groups = document.typed.proxy_groups.len(),
            rules = document.typed.rules.len(),
            "validating config document with mihomo"
        );
        let yaml = CoreConfigStore::runtime_config_yaml(&document.typed)?;
        let binary_path = self.resolve_mihomo_binary_for_config_test().await?;
        self.test_config_yaml_with_binary(binary_path, &yaml, scope)
            .await
    }

    async fn resolve_mihomo_binary_for_config_test(&self) -> AppResult<PathBuf> {
        #[cfg(test)]
        {
            // app 层单元测试不依赖真实 mihomo；真实二进制调用的参数与错误映射由 core::config_test 覆盖。
            return Ok(self.paths.cores_dir.join(if cfg!(target_os = "windows") {
                "mihomo.exe"
            } else {
                "mihomo"
            }));
        }

        #[cfg(not(test))]
        {
            let detector = MihomoRuntimeDetector;
            let info = detector
                .detect_runtime(RuntimeDetectionOptions::new(
                    Some(self.paths.config_dir.clone()),
                    self.paths.cores_dir.clone(),
                    None,
                ))
                .await?;
            let Some(binary_path) = info.binary_path else {
                return Err(
                    ProcessError::InvalidState("无法校验配置：未找到 mihomo 核心".into()).into(),
                );
            };
            if !info.executable {
                return Err(ProcessError::PermissionDenied(binary_path).into());
            }
            Ok(binary_path)
        }
    }

    async fn test_config_yaml_with_binary(
        &self,
        binary_path: PathBuf,
        yaml: &str,
        scope: &str,
    ) -> AppResult<()> {
        #[cfg(test)]
        {
            // 单元测试环境没有跨平台的真实 mihomo 可执行文件；外部进程参数构造和失败映射在 core::config_test
            // 中单测覆盖，app 层测试保留命令路由和仓储行为的确定性。
            let _ = (binary_path, yaml, scope);
            return Ok(());
        }

        #[cfg(not(test))]
        {
            tracing::debug!(scope, "validating config through mihomo binary");
            test_mihomo_config(
                MihomoConfigTestOptions::new(
                    binary_path,
                    self.paths.cores_dir.clone(),
                    vec![
                        self.paths.config_dir.clone(),
                        self.paths.data_dir.clone(),
                        self.paths.cache_dir.clone(),
                    ],
                ),
                yaml,
            )
            .await
        }
    }
}

fn document_enables_tun(document: &ConfigDocument) -> bool {
    document
        .typed
        .tun
        .as_ref()
        .and_then(|tun| tun.enable)
        .unwrap_or(false)
}

fn ensure_runtime_dns_defaults(document: &mut ConfigDocument) {
    let Some(dns) = document.typed.dns.as_mut() else {
        return;
    };
    // mihomo 新版本会校验 DNS 段里的上游列表，即使 dns.enable=false；
    // 运行配置补最小默认值，避免首次启动的空 DNS 段触发 fatal，同时不回写用户 profile。
    if dns.default_nameserver.is_empty() {
        dns.default_nameserver.push("223.5.5.5".to_string());
    }
    if dns.nameserver.is_empty() {
        dns.nameserver
            .push("https://dns.alidns.com/dns-query".to_string());
    }
}

fn active_subscription_name(subscriptions: &[SubscriptionMergeInput]) -> String {
    subscriptions
        .iter()
        .find(|source| source.enabled)
        .map(|source| source.display_name.trim())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "本地配置".to_string())
}

#[derive(Clone)]
pub struct MihomoClientFactory {
    core_config_store: Arc<CoreConfigStore>,
}

impl MihomoClientFactory {
    pub fn new(core_config_store: Arc<CoreConfigStore>) -> Self {
        Self { core_config_store }
    }

    pub fn client(&self) -> AppResult<MihomoHttpClient> {
        let document = self.core_config_store.load_user_config().ok();
        Ok(MihomoHttpClient::new(Self::endpoint_from_document(
            document.as_ref(),
        )))
    }

    pub fn stream_client(&self) -> AppResult<MihomoStreamClient> {
        let document = self.core_config_store.load_user_config().ok();
        Ok(MihomoStreamClient::new(Self::endpoint_from_document(
            document.as_ref(),
        )))
    }

    pub fn endpoint_from_document(document: Option<&ConfigDocument>) -> MihomoEndpoint {
        let controller = document
            .and_then(|document| document.typed.global.external_controller.as_deref())
            .unwrap_or("127.0.0.1:19090");
        MihomoEndpoint {
            base_url: normalize_controller_url(controller),
            secret: document
                .and_then(|document| document.typed.global.secret.as_ref())
                .cloned(),
        }
    }
}

fn runtime_status_from_mihomo(status: &MihomoServiceStatus) -> RuntimeStatus {
    match status.phase {
        MihomoServicePhase::Idle | MihomoServicePhase::Ready | MihomoServicePhase::Preparing => {
            RuntimeStatus::Idle
        }
        MihomoServicePhase::Starting => RuntimeStatus::Starting,
        MihomoServicePhase::Running => RuntimeStatus::Running,
        MihomoServicePhase::Stopping => RuntimeStatus::Stopping,
        MihomoServicePhase::Failed => RuntimeStatus::Failed {
            message: status
                .last_error
                .as_deref()
                .map(redact_log_value)
                .unwrap_or_else(|| "mihomo 状态异常".into()),
        },
    }
}

fn core_service_paths(paths: &AppPaths) -> CoreServicePaths {
    CoreServicePaths {
        config_dir: paths.config_dir.clone(),
        data_dir: paths.data_dir.clone(),
        cache_dir: paths.cache_dir.clone(),
        cores_dir: paths.cores_dir.clone(),
        logs_dir: paths.logs_dir.clone(),
    }
}

fn normalize_controller_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

#[cfg(not(test))]
fn capture_system_proxy_snapshot_for_runtime(
    port: u32,
) -> AppResult<air_platform::system_proxy::SystemProxySnapshot> {
    air_platform::system_proxy::capture_system_proxy_snapshot(port)
}

#[cfg(not(test))]
fn enable_local_system_proxy_for_runtime(
    port: u32,
) -> AppResult<air_platform::system_proxy::SystemProxyUpdate> {
    air_platform::system_proxy::enable_local_system_proxy(port)
}

#[cfg(test)]
fn capture_system_proxy_snapshot_for_runtime(
    port: u32,
) -> AppResult<air_platform::system_proxy::SystemProxySnapshot> {
    Ok(air_platform::system_proxy::SystemProxySnapshot {
        managed_port: port,
        services: vec![air_platform::system_proxy::SystemProxyServiceSnapshot {
            service: "Wi-Fi".into(),
            web: air_platform::system_proxy::MacosProxyState::default(),
            secure_web: air_platform::system_proxy::MacosProxyState::default(),
            socks: air_platform::system_proxy::MacosProxyState::default(),
        }],
    })
}

#[cfg(test)]
fn enable_local_system_proxy_for_runtime(
    port: u32,
) -> AppResult<air_platform::system_proxy::SystemProxyUpdate> {
    if port == 19091 {
        return Err(
            air_error::PlatformError::OperationFailed("test system proxy failure".into()).into(),
        );
    }
    Ok(air_platform::system_proxy::SystemProxyUpdate {
        services: vec!["Wi-Fi".into()],
    })
}

#[cfg(not(test))]
fn restore_system_proxy_snapshot_for_runtime(
    snapshot: &air_platform::system_proxy::SystemProxySnapshot,
) -> AppResult<air_platform::system_proxy::SystemProxyRestore> {
    air_platform::system_proxy::restore_system_proxy_snapshot(snapshot)
}

#[cfg(test)]
fn restore_system_proxy_snapshot_for_runtime(
    snapshot: &air_platform::system_proxy::SystemProxySnapshot,
) -> AppResult<air_platform::system_proxy::SystemProxyRestore> {
    Ok(air_platform::system_proxy::SystemProxyRestore {
        restored_services: snapshot
            .services
            .iter()
            .map(|service| service.service.clone())
            .collect(),
        skipped_services: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use air_mihomo::subscriptions::{SubscriptionSource, SubscriptionUpdateResult};
    use air_storage::AppPaths;

    #[test]
    fn endpoint_is_derived_from_core_config_without_hardcoded_secret() {
        let document =
            ConfigDocument::parse("external-controller: 127.0.0.1:9099\nsecret: secret-token\n")
                .unwrap();

        let endpoint = MihomoClientFactory::endpoint_from_document(Some(&document));

        assert_eq!(endpoint.base_url, "http://127.0.0.1:9099");
        assert_eq!(endpoint.secret.as_deref(), Some("secret-token"));
    }

    #[test]
    fn tun_enable_flag_controls_privileged_core_start_requirement() {
        let enabled = ConfigDocument::parse("tun:\n  enable: true\n").unwrap();
        let disabled = ConfigDocument::parse("tun:\n  enable: false\n").unwrap();
        let absent = ConfigDocument::parse("mixed-port: 7890\n").unwrap();

        assert!(document_enables_tun(&enabled));
        assert!(!document_enables_tun(&disabled));
        assert!(!document_enables_tun(&absent));
    }

    #[test]
    fn snapshot_store_keeps_latest_runtime_state() {
        let runtime = Arc::new(AppRuntime::new().unwrap());
        let store = AppStateStore::new(runtime, AppSnapshot::default());
        let snapshot = AppSnapshot {
            runtime: RuntimeStatus::Running,
            ..AppSnapshot::default()
        };

        store.replace_snapshot(snapshot);

        assert!(matches!(store.snapshot().runtime, RuntimeStatus::Running));
    }

    #[test]
    fn detection_options_probe_controller_only_after_runtime_running() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();

        let idle_options = services.detection_options().unwrap();
        assert_eq!(idle_options.controller_addr, None);

        services
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let running_options = services.detection_options().unwrap();
        assert_eq!(
            running_options.controller_addr.as_deref(),
            Some("http://127.0.0.1:19090")
        );
    }

    #[test]
    fn save_current_config_writes_core_config_with_backup_and_validation() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();

        services
            .save_current_config("mixed-port: 19090\nfuture-top:\n  keep: true\n")
            .unwrap();
        services
            .save_current_config("mixed-port: 19091\nfuture-top:\n  keep: true\n")
            .unwrap();

        let saved = services.core_config_store.load_user_config().unwrap();
        assert_eq!(saved.typed.global.mixed_port, Some(19091));
        assert!(saved.typed.extensions.contains_key("future-top"));
        assert!(
            temp.path()
                .join("data/backups/core.common.config.yaml.bak")
                .exists()
        );
    }

    #[test]
    fn selected_subscription_document_reads_enabled_cached_config() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();
        let mut source =
            SubscriptionSource::remote("work", "Work", "https://example.test/sub.yaml");
        source.enabled = true;
        services.subscription_store.save_source(source).unwrap();
        services
            .subscription_store
            .record_update_result(
                "work",
                SubscriptionUpdateResult::success(1000, 128),
                Some(
                    b"proxies:\n  - name: node-a\n    type: ss\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies:\n      - node-a\n",
                ),
            )
            .unwrap();

        let document = services
            .selected_subscription_document()
            .unwrap()
            .expect("enabled subscription cache should parse as config");

        assert_eq!(document.typed.proxy_groups[0].name, "Proxy");
        assert_eq!(document.typed.proxies[0].name, "node-a");
    }

    #[test]
    fn selected_subscription_document_does_not_fallback_to_disabled_source() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();
        let mut source =
            SubscriptionSource::remote("backup", "Backup", "https://example.test/sub.yaml");
        source.enabled = false;
        services.subscription_store.save_source(source).unwrap();
        services
            .subscription_store
            .record_update_result(
                "backup",
                SubscriptionUpdateResult::success(1000, 128),
                Some(
                    b"proxies:\n  - name: node-a\n    type: ss\nproxy-groups:\n  - name: Backup\n    type: select\n    proxies:\n      - node-a\n",
                ),
            )
            .unwrap();

        let document = services.selected_subscription_document().unwrap();

        assert!(document.is_none());
    }

    #[test]
    fn core_config_store_writes_runtime_config_to_config_dir() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();

        let mut document = services.core_config_store.load_user_config().unwrap();
        ensure_runtime_dns_defaults(&mut document);
        let config_path = services
            .core_config_store
            .write_runtime_config(&document.typed, &[])
            .unwrap();
        let runtime_config = std::fs::read_to_string(&config_path).unwrap();

        assert_eq!(
            config_path,
            paths.config_dir.join("core.runtime.config.yaml")
        );
        assert!(runtime_config.contains("default-nameserver"));
        assert!(runtime_config.contains("223.5.5.5"));
        assert!(runtime_config.contains("https://dns.alidns.com/dns-query"));
    }

    #[test]
    fn active_override_script_runs_before_runtime_config_write() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();
        services
            .save_override_script(
                r#"
function override(subscriptionName, config) {
  config["mixed-port"] = 19191;
  config.rules.push("MATCH,DIRECT");
  return config;
}
"#,
                true,
            )
            .unwrap();

        let path = services.write_runtime_config().unwrap();
        let runtime_config = std::fs::read_to_string(&path).unwrap();

        assert_eq!(path, paths.config_dir.join("core.runtime.config.yaml"));
        assert!(runtime_config.contains("mixed-port: 19191"));
        assert!(runtime_config.contains("MATCH,DIRECT"));
    }

    #[test]
    fn current_mixed_port_uses_effective_runtime_config() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();
        services
            .save_override_script(
                r#"
function override(subscriptionName, config) {
  config["mixed-port"] = 19191;
  return config;
}
"#,
                true,
            )
            .unwrap();

        assert_eq!(services.current_mixed_port().unwrap(), Some(19191));
    }

    #[test]
    fn system_proxy_sync_captures_snapshot_once_and_updates_managed_port() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();
        let mut settings = services.load_settings().unwrap();
        settings.system_proxy_enabled = true;
        services.save_settings(&settings).unwrap();

        services.save_current_config("mixed-port: 9870\n").unwrap();
        services.sync_system_proxy_to_current_config().unwrap();
        let mut snapshot = services.system_proxy_snapshots.load().unwrap().unwrap();
        snapshot.services[0].web.server = Some("proxy.example.test".into());
        services.system_proxy_snapshots.save(&snapshot).unwrap();

        services.save_current_config("mixed-port: 19090\n").unwrap();
        services.sync_system_proxy_to_current_config().unwrap();
        let updated = services.system_proxy_snapshots.load().unwrap().unwrap();

        assert_eq!(updated.managed_port, 19090);
        assert_eq!(
            updated.services[0].web.server.as_deref(),
            Some("proxy.example.test")
        );
        assert!(paths.data_dir.join("system-proxy-snapshot.json").exists());
    }

    #[test]
    fn system_proxy_sync_is_opt_in_and_default_off() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();

        services.save_current_config("mixed-port: 9870\n").unwrap();
        services.sync_system_proxy_to_current_config().unwrap();

        assert!(services.system_proxy_snapshots.load().unwrap().is_none());
        assert!(!paths.data_dir.join("system-proxy-snapshot.json").exists());
    }

    #[test]
    fn system_proxy_sync_disabled_restores_existing_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();
        let snapshot = capture_system_proxy_snapshot_for_runtime(9870).unwrap();
        services.system_proxy_snapshots.save(&snapshot).unwrap();

        services.save_current_config("mixed-port: 9870\n").unwrap();
        services.sync_system_proxy_to_current_config().unwrap();

        assert!(services.system_proxy_snapshots.load().unwrap().is_none());
        assert!(!paths.data_dir.join("system-proxy-snapshot.json").exists());
    }

    #[test]
    fn system_proxy_sync_restores_and_deletes_snapshot_when_enable_fails() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();
        let mut settings = services.load_settings().unwrap();
        settings.system_proxy_enabled = true;
        services.save_settings(&settings).unwrap();
        let mut snapshot = capture_system_proxy_snapshot_for_runtime(9870).unwrap();
        snapshot.services[0].web.server = Some("proxy.example.test".into());
        services.system_proxy_snapshots.save(&snapshot).unwrap();

        services.save_current_config("mixed-port: 19091\n").unwrap();
        services.sync_system_proxy_to_current_config().unwrap();

        assert!(services.system_proxy_snapshots.load().unwrap().is_none());
    }

    #[test]
    fn system_proxy_sync_without_mixed_port_restores_and_deletes_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();
        let mut settings = services.load_settings().unwrap();
        settings.system_proxy_enabled = true;
        services.save_settings(&settings).unwrap();
        let snapshot = capture_system_proxy_snapshot_for_runtime(9870).unwrap();
        services.system_proxy_snapshots.save(&snapshot).unwrap();

        services.save_current_config("port: 9090\n").unwrap();
        services.sync_system_proxy_to_current_config().unwrap();

        assert!(services.system_proxy_snapshots.load().unwrap().is_none());
        assert!(!paths.data_dir.join("system-proxy-snapshot.json").exists());
    }

    #[test]
    fn disable_system_proxy_restores_snapshot_and_removes_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();
        let snapshot = capture_system_proxy_snapshot_for_runtime(9870).unwrap();
        services.system_proxy_snapshots.save(&snapshot).unwrap();

        services.disable_system_proxy();

        assert!(services.system_proxy_snapshots.load().unwrap().is_none());
        assert!(!paths.data_dir.join("system-proxy-snapshot.json").exists());
    }

    #[test]
    fn debug_override_script_generates_yaml_without_writing_runtime_config() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();

        let yaml = services
            .preview_override_script(
                r#"
function override(_, config) {
  config["mixed-port"] = 18080;
  return config;
}
"#,
            )
            .unwrap();

        assert!(yaml.contains("mixed-port: 18080"));
        assert!(!paths.config_dir.join("core.runtime.config.yaml").exists());
    }

    #[test]
    fn launch_config_uses_cache_core_as_core_working_dir() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths.clone()).unwrap();
        std::fs::write(paths.cores_dir.join("mihomo"), b"fake").unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let launch = runtime.block_on(services.launch_config()).unwrap();

        assert_eq!(launch.working_dir, paths.cores_dir);
        assert_eq!(
            launch.config_path,
            paths.config_dir.join("core.runtime.config.yaml")
        );
    }

    #[test]
    fn startup_uses_single_core_config_projection() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );

        let services = AppServices::with_paths(paths).unwrap();
        assert_eq!(
            services.snapshots.snapshot().active_profile.as_deref(),
            Some("core.common.config.yaml")
        );
        let document = services.core_config_store.load_user_config().unwrap();
        assert_eq!(document.typed.global.mixed_port, Some(9870));
    }

    #[test]
    fn shutdown_path_stops_core_through_owned_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();

        services.stop_core_before_exit().unwrap();
        services.stop_core_before_exit().unwrap();

        assert!(matches!(
            services.snapshots.snapshot().runtime,
            RuntimeStatus::Idle
        ));
    }
}
