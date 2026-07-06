use super::lifecycle::*;
use super::*;

impl Shell {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        force_start_core: bool,
        single_instance_events: Receiver<SingleInstanceEvent>,
    ) -> Self {
        // 首屏默认进入订阅页；其他页面的输入控件和订阅回调在路由激活时按需创建。
        let (app_settings, command_router, initial_snapshot, app_state_store) = load_app_backing();
        if command_router.is_some() {
            sync_platform_autostart(app_settings.autostart);
        }

        let settings = settings::SettingsPageState::new(app_settings);
        let settings_inputs = create_settings_inputs(settings.settings(), window, cx);
        let settings_subscriptions = settings_input_subscriptions(&settings_inputs, window, cx);
        let system_theme_mode = ShellThemeMode::from_window_appearance(window.appearance());
        let initial_theme = ShellThemePreference::from(settings.settings().theme);
        apply_theme_preference(initial_theme, system_theme_mode, window, cx);
        let appearance_subscription = cx.observe_window_appearance(window, |this, window, cx| {
            this.system_theme_mode = ShellThemeMode::from_window_appearance(window.appearance());
            if this.theme_preference == ShellThemePreference::System {
                Theme::sync_system_appearance(Some(window), cx);
                cx.notify();
            }
        });

        if let (Some(router), Some(snapshot_store)) =
            (command_router.as_ref(), app_state_store.clone())
        {
            let mut events = router.services().runtime.subscribe();
            let shell = cx.entity().clone();
            cx.spawn_in(window, async move |_, window| {
                loop {
                    let event = match events.recv().await {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                skipped,
                                "app event receiver lagged; applying latest snapshot"
                            );
                            AppEvent::SnapshotChanged(snapshot_store.snapshot())
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };

                    if event.kind() != "MihomoStreamEvent" {
                        tracing::info!(
                            event_kind = event.kind(),
                            event_payload = %event.log_payload(),
                            "ui received backend event"
                        );
                    }

                    let updated = window.update({
                        let shell = shell.clone();
                        move |window, cx| {
                            let _ = shell.update(cx, |shell, cx| {
                                let subscription_projection_changed =
                                    matches!(&event, AppEvent::SubscriptionStateChanged(_));
                                let effect = shell.apply_app_event(event);
                                shell.sync_active_page_inputs(window, cx);
                                shell.notify_connection_result(window, cx);
                                if subscription_projection_changed {
                                    shell.notify_subscription_diagnostics(window, cx);
                                }
                                push_app_event_notice(window, cx, &effect);
                                if effect.should_notify() {
                                    cx.notify();
                                }
                            });
                        }
                    });
                    if updated.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        spawn_subscription_refresh_loop(cx);
        let (tray_handle, tray_events) = create_tray();
        let hide_window_on_startup =
            should_hide_window_on_startup(settings.settings(), tray_handle.is_supported());
        if tray_handle.is_supported() {
            spawn_tray_event_loop(tray_events, window, cx);
        }
        spawn_single_instance_event_loop(single_instance_events, window, cx);
        if hide_window_on_startup {
            spawn_initial_window_hide(window, cx);
        }
        dispatch_startup_prepare(
            command_router.as_ref(),
            should_start_core_on_startup(settings.settings(), force_start_core),
        );
        let initial_subscriptions = if hide_window_on_startup {
            // 静默启动会立即隐藏到托盘，首屏页面不可见时不预加载订阅投影，恢复窗口时再按路由装载。
            subscriptions::SubscriptionPageState::empty()
        } else {
            load_initial_subscriptions(command_router.as_ref())
        };
        let initial_config_editor = if hide_window_on_startup {
            // 配置编辑草稿包含完整 YAML 投影；隐藏态先保持空草稿，避免启动后马上占用页面内存。
            config_editor::ConfigEditorPageState::empty()
        } else {
            load_initial_config_editor(command_router.as_ref())
        };
        let initial_tun_enabled = load_saved_tun_enabled(command_router.as_ref()).unwrap_or(false);
        let initial_runtime_mode =
            load_saved_runtime_mode(command_router.as_ref()).unwrap_or_else(|| {
                status_runtime_mode_value(&initial_config_editor.view_model().draft.global.mode)
            });

        let mut shell = Self {
            active_route: AppRoute::Subscriptions,
            // GPUI state 只保存窗口渲染所需的局部状态；业务能力仍在 app/domain/service 层。
            snapshot: initial_snapshot,
            monitor: monitor::MonitorPageState::default(),
            log_monitoring_active: false,
            traffic_monitoring_active: false,
            connections_monitoring_active: false,
            status_tun_enabled: initial_tun_enabled,
            // 状态栏展示的是已知运行模式，不能绑定到配置页草稿；路由切换会销毁草稿状态。
            status_runtime_mode: initial_runtime_mode,
            page_states_suspended_for_tray: false,
            tray_cleanup_generation: 0,
            monitor_log_scroll_handle: VirtualListScrollHandle::new(),
            log_runtime: None,
            connection_detail_editor_contents: String::new(),
            subscription_yaml_editor_contents: String::new(),
            rules_proxy: rules::RulesProxyPageState::empty(),
            rules_proxy_scroll_handle: VirtualListScrollHandle::new(),
            rules_proxy_runtime: None,
            override_script: override_script::OverrideScriptPageState::default(),
            override_preview_editor_contents: String::new(),
            override_script_runtime: None,
            groups: proxy_groups::GroupPageState::empty(),
            group_runtime: None,
            connections: connections::ConnectionsPageState::default(),
            connections_runtime: None,
            subscriptions: initial_subscriptions,
            subscription_runtime: None,
            subscription_diagnostic_notices: BTreeSet::new(),
            settings,
            command_router,
            settings_inputs,
            _settings_subscriptions: settings_subscriptions,
            _shutdown_subscription: None,
            config_editor: initial_config_editor,
            config_runtime: None,
            config_save_notices: BTreeSet::new(),
            theme_preference: initial_theme,
            system_theme_mode,
            _appearance_subscription: appearance_subscription,
            _tray_handle: tray_handle,
            pending_commands: BTreeMap::new(),
            pending_after_config_save: BTreeMap::new(),
            core_service_confirmation: None,
            app_quit_requested: false,
        };
        shell.reconcile_traffic_monitoring();
        shell.reconcile_connections_monitoring_focus();
        if hide_window_on_startup {
            let generation = shell.begin_tray_suspension();
            shell.destroy_all_page_states_for_tray();
            air_telemetry::memory::shrink_process_memory("silent-start-tray-hide");
            spawn_tray_resource_cleanup(window, cx, generation);
        }
        shell
    }
    pub(super) fn navigate(
        &mut self,
        route: AppRoute,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_route = self.active_route;
        self.active_route = route;
        if previous_route != route {
            if previous_route == AppRoute::Settings {
                self.clear_config_save_notices(window, cx);
            }
            self.destroy_route_state(previous_route);
            if !self.page_states_suspended_for_tray {
                self.hydrate_route_state(route, window, cx);
            }
            self.reconcile_log_monitoring_focus();
            self.reconcile_traffic_monitoring();
            self.reconcile_proxy_groups_backend();
            self.reconcile_rules_backend();
            self.reconcile_connections_monitoring_focus();
        }
    }

    pub(super) fn destroy_route_state(&mut self, route: AppRoute) {
        // 页面状态只服务当前可见路由；离开页面时立即丢弃列表、日志、弹窗和编辑草稿。
        // 这样可以避免后台隐藏页面继续持有大量运行态数据。
        match route {
            AppRoute::Logs => {
                self.monitor = monitor::MonitorPageState::default();
                self.log_runtime = None;
            }
            AppRoute::RulesProxy => {
                self.rules_proxy = rules::RulesProxyPageState::empty();
                self.rules_proxy_runtime = None;
            }
            AppRoute::OverrideScript => {
                self.override_script = override_script::OverrideScriptPageState::default();
                self.override_script_runtime = None;
                self.override_preview_editor_contents.clear();
            }
            AppRoute::ProxyGroups => {
                self.groups = proxy_groups::GroupPageState::empty();
                self.group_runtime = None;
            }
            AppRoute::Connections => {
                self.connections = connections::ConnectionsPageState::default();
                self.connections_runtime = None;
                self.connection_detail_editor_contents.clear();
            }
            AppRoute::Subscriptions => {
                self.subscriptions = subscriptions::SubscriptionPageState::empty();
                self.subscription_runtime = None;
                self.subscription_yaml_editor_contents.clear();
            }
            AppRoute::Profiles => {
                self.config_editor = config_editor::ConfigEditorPageState::empty();
                self.config_runtime = None;
            }
            AppRoute::Settings => {
                self.settings = settings::SettingsPageState::new(self.settings.settings().clone());
                self.config_editor = config_editor::ConfigEditorPageState::empty();
                self.config_runtime = None;
            }
        }
        tracing::info!(route = route.id(), "inactive page state destroyed");
    }

    pub(super) fn begin_tray_suspension(&mut self) -> u64 {
        self.page_states_suspended_for_tray = true;
        self.tray_cleanup_generation = self.tray_cleanup_generation.wrapping_add(1);
        self.reconcile_log_monitoring_focus();
        self.reconcile_traffic_monitoring();
        self.reconcile_connections_monitoring_focus();
        tracing::info!(
            generation = self.tray_cleanup_generation,
            delay_ms = TRAY_RESOURCE_RELEASE_DELAY.as_millis(),
            "tray suspension started; immediate page cleanup and delayed memory shrink scheduled"
        );
        self.tray_cleanup_generation
    }

    pub(super) fn destroy_all_page_states_for_tray(&mut self) {
        for route in [
            AppRoute::RulesProxy,
            AppRoute::OverrideScript,
            AppRoute::ProxyGroups,
            AppRoute::Connections,
            AppRoute::Subscriptions,
            AppRoute::Logs,
            AppRoute::Profiles,
            AppRoute::Settings,
        ] {
            self.destroy_route_state(route);
        }
        // 托盘隐藏时没有任何页面需要流式数据，统一通过 reconcile 停止订阅。
        self.reconcile_log_monitoring_focus();
        self.reconcile_traffic_monitoring();
        self.reconcile_connections_monitoring_focus();
        tracing::info!("all page states destroyed for tray minimization");
    }

    pub(super) fn clear_config_save_notices(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for group in self.config_save_notices.iter().copied().collect::<Vec<_>>() {
            super::components::remove_persistent_global_notice(
                window,
                cx,
                config_save_notice_key(group),
            );
        }
        self.config_save_notices.clear();
    }

    pub(super) fn hydrate_route_state(
        &mut self,
        route: AppRoute,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 重新打开页面时从当前 app/service 投影装载，而不是复用离开前的 GUI 状态。
        match route {
            AppRoute::Logs => {
                self.log_runtime = Some(create_log_runtime(window, cx));
            }
            AppRoute::RulesProxy => {
                self.rules_proxy = rules::RulesProxyPageState::empty();
                self.rules_proxy_runtime = Some(create_rules_proxy_runtime(window, cx));
            }
            AppRoute::OverrideScript => {
                let source = load_initial_override_script(self.command_router.as_ref());
                self.override_script = override_script::OverrideScriptPageState::new(
                    self.settings.settings().override_script_enabled,
                    source.clone(),
                );
                self.override_script_runtime =
                    Some(create_override_script_runtime(source, window, cx));
            }
            AppRoute::ProxyGroups => {
                self.groups = proxy_groups::GroupPageState::empty();
                self.group_runtime = Some(create_group_runtime(&self.groups, window, cx));
            }
            AppRoute::Connections => {
                self.connections = connections::ConnectionsPageState::default();
                self.connections_runtime = Some(create_connections_runtime(window, cx));
            }
            AppRoute::Subscriptions => {
                self.subscriptions = load_initial_subscriptions(self.command_router.as_ref());
                self.subscription_runtime =
                    Some(create_subscription_runtime(&self.subscriptions, window, cx));
            }
            AppRoute::Profiles | AppRoute::Settings => {
                self.config_editor = load_initial_config_editor(self.command_router.as_ref());
                self.settings = settings::SettingsPageState::new(self.settings.settings().clone());
                self.config_runtime = Some(create_config_runtime(&self.config_editor, window, cx));
            }
        }
        tracing::info!(route = route.id(), "active page state hydrated");
    }

    pub(super) fn reconcile_proxy_groups_backend(&mut self) {
        if self.active_route != AppRoute::ProxyGroups
            || !matches!(self.snapshot.runtime, RuntimeStatus::Running)
        {
            return;
        }
        let refresh_pending = self
            .pending_commands
            .values()
            .any(|command| matches!(command, AppCommand::RefreshProxies));
        if !refresh_pending {
            self.dispatch_command(AppCommand::RefreshProxies);
        }
    }

    pub(super) fn reconcile_rules_backend(&mut self) {
        if self.active_route != AppRoute::RulesProxy
            || !matches!(self.snapshot.runtime, RuntimeStatus::Running)
        {
            return;
        }
        let refresh_pending = self
            .pending_commands
            .values()
            .any(|command| matches!(command, AppCommand::RefreshRules));
        if !refresh_pending {
            self.dispatch_command(AppCommand::RefreshRules);
        }
    }

    pub(super) fn reconcile_connections_monitoring_focus(&mut self) {
        let should_run = should_run_connections_monitoring(
            self.active_route,
            self.page_states_suspended_for_tray,
            &self.snapshot.runtime,
        );
        match (should_run, self.connections_monitoring_active) {
            (true, false) => {
                self.connections.start_stream();
                self.connections_monitoring_active = true;
                self.dispatch_command(AppCommand::StartConnectionsMonitoring);
            }
            (false, true) => {
                self.connections.stop_stream();
                self.connections_monitoring_active = false;
                self.dispatch_command(AppCommand::StopConnectionsMonitoring);
            }
            _ => {}
        }
    }

    pub(super) fn reconcile_traffic_monitoring(&mut self) {
        let should_run = should_run_traffic_monitoring(
            self.active_route,
            self.page_states_suspended_for_tray,
            &self.snapshot.runtime,
        );
        match (should_run, self.traffic_monitoring_active) {
            (true, false) => {
                self.traffic_monitoring_active = true;
                self.dispatch_command(AppCommand::StartTrafficMonitoring);
            }
            (false, true) => {
                self.traffic_monitoring_active = false;
                self.monitor.release_transient_stream_state();
                self.dispatch_command(AppCommand::StopTrafficMonitoring);
            }
            _ => {}
        }
    }

    pub(super) fn reconcile_log_monitoring_focus(&mut self) {
        let should_run =
            should_run_log_monitoring(self.active_route, self.page_states_suspended_for_tray);
        match (should_run, self.log_monitoring_active) {
            (true, false) => {
                self.log_monitoring_active = true;
                self.dispatch_command(AppCommand::StartLogMonitoring);
            }
            (false, true) => {
                self.log_monitoring_active = false;
                self.monitor.stop_streams();
                self.dispatch_command(AppCommand::StopLogMonitoring);
            }
            _ => {}
        }
    }

    pub(crate) fn dispatch_command(&mut self, command: AppCommand) {
        // UI 只表达用户意图；核心生命周期、配置写入和日志文件读取都交给 app 层命令路由。
        tracing::info!(
            command_kind = command.kind(),
            command_payload = %command.log_payload(),
            "ui dispatching command to backend"
        );
        if let Some(router) = &self.command_router {
            let id = router.dispatch(command.clone());
            self.pending_commands.insert(id, command);
        } else {
            tracing::warn!("command router unavailable; command only recorded for diagnostics");
        }
    }

    pub(super) fn dispatch_command_after_config_save(
        &mut self,
        save_command: AppCommand,
        after_save: Option<AppCommand>,
    ) {
        tracing::info!(
            command_kind = save_command.kind(),
            command_payload = %save_command.log_payload(),
            "ui dispatching config save command to backend"
        );
        if let Some(router) = &self.command_router {
            let id = router.dispatch(save_command.clone());
            self.pending_commands.insert(id, save_command);
            if let Some(after_save) = after_save {
                self.pending_after_config_save.insert(id, after_save);
            }
        } else {
            tracing::warn!(
                "command router unavailable; config save command only recorded for diagnostics"
            );
        }
    }

    pub(super) fn handle_tray_event(
        &mut self,
        event: TrayEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TrayEvent::ToggleWindow => {
                tracing::info!("tray requested window toggle");
                self.toggle_window_from_tray(window, cx);
            }
            TrayEvent::ShowWindow => {
                tracing::info!("tray requested window show");
                self.show_window_from_tray(window, cx);
            }
            TrayEvent::HideWindow => {
                tracing::info!("tray requested window hide");
                self.hide_window_from_tray(window, cx);
            }
            TrayEvent::StartCore => {
                tracing::info!("tray requested core start");
                self.dispatch_command(AppCommand::StartCore);
                cx.notify();
            }
            TrayEvent::StopCore => {
                tracing::info!("tray requested core stop");
                self.dispatch_command(AppCommand::StopCore);
                cx.notify();
            }
            TrayEvent::Quit => {
                tracing::info!("tray requested app quit");
                self.stop_core_before_app_exit();
                cx.quit();
            }
        }
    }

    pub(super) fn toggle_window_from_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 鎵樼洏绾跨▼鏃犳硶鍙潬璇诲彇 GPUI 绐楀彛鐘舵€侊紝鍥犳鍙淳鍙戝垏鎹㈡剰鍥撅紱鐪熸鐨勫彲瑙佹€у垽鏂斁鍥?UI
        // 窗口可见性仍在 UI 线程确认，并把最小化也视为需要恢复的状态。
        match air_platform::window::is_window_visible(window) {
            Ok(true) => self.hide_window_from_tray(window, cx),
            Ok(false) => self.show_window_from_tray(window, cx),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to query window visibility from tray; restoring window"
                );
                self.show_window_from_tray(window, cx);
            }
        }
    }

    pub(super) fn show_window_from_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tray_cleanup_generation = self.tray_cleanup_generation.wrapping_add(1);
        // 重新显示时统一销毁旧页面状态并重建当前页，确保可见窗口只保留当前路由所需内存。
        self.destroy_all_page_states_for_tray();
        self.page_states_suspended_for_tray = false;
        self.hydrate_route_state(self.active_route, window, cx);
        self.reconcile_traffic_monitoring();
        self.reconcile_log_monitoring_focus();
        self.reconcile_connections_monitoring_focus();
        self.reconcile_proxy_groups_backend();
        self.reconcile_rules_backend();
        if let Err(error) = air_platform::window::show_window(window) {
            tracing::warn!(%error, "failed to show window from tray");
        }
        cx.activate(true);
        window.activate_window();
    }

    pub(super) fn show_window_from_single_instance(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match air_platform::window::is_window_visible(window) {
            Ok(false) => self.show_window_from_tray(window, cx),
            Ok(true) => {
                // 第二次启动只负责把已有窗口带到前台；窗口已经可见时不能重建页面状态，
                // 否则会意外清空当前筛选、弹窗和编辑草稿。
                if let Err(error) = air_platform::window::show_window(window) {
                    tracing::warn!(%error, "failed to foreground existing window");
                }
                cx.activate(true);
                window.activate_window();
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to query window visibility for single instance restore"
                );
                self.show_window_from_tray(window, cx);
            }
        }
    }

    pub(super) fn hide_window_from_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_config_save_notices(window, cx);
        let generation = self.begin_tray_suspension();
        self.destroy_all_page_states_for_tray();
        air_telemetry::memory::shrink_process_memory("tray-hide-immediate");
        spawn_tray_resource_cleanup(window, cx, generation);
        if let Err(error) = air_platform::window::hide_window(window) {
            tracing::warn!(%error, "failed to hide window from tray");
        }
    }

    pub(super) fn stop_core_before_app_exit(&self) {
        let Some(router) = self.command_router.as_ref() else {
            return;
        };
        // 退出收尾直接控制核心进程；先取消同一条 core 长任务，降低启动/重启命令和 stop 并发的概率。
        router.cancel_registered(&AppCommand::StartCore);
        if let Err(error) = router.services().stop_core_before_exit() {
            tracing::warn!(
                %error,
                "failed to stop mihomo core before app exit; continuing shutdown"
            );
        }
    }

    pub(super) fn request_app_quit(&mut self) {
        self.app_quit_requested = true;
        mark_app_quit_requested();
    }

    pub(super) fn should_close_window_for_intent(&self) -> bool {
        should_close_window_for_intent(
            self.settings.settings(),
            self.app_quit_requested || app_quit_requested(),
        )
    }
}
