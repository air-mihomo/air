use super::*;

impl Shell {
    pub(crate) fn apply_app_event(&mut self, event: AppEvent) -> ShellAppEventEffect {
        // 事件回填集中在 Shell 边界；页面只消费当前路由需要的投影或流事件。
        if !self.page_states_suspended_for_tray
            && self.active_route == AppRoute::OverrideScript
            && let AppEvent::OverridePreviewGenerated { contents } = &event
        {
            self.override_script.set_preview(contents.clone());
        }
        if let AppEvent::UserVisibleError { message } = &event {
            if !self.page_states_suspended_for_tray
                && self.active_route == AppRoute::OverrideScript
                && self.override_debug_pending()
            {
                self.override_script
                    .set_preview_error(redact_log_value(message));
            }
            if !self.page_states_suspended_for_tray && self.active_route == AppRoute::Subscriptions
            {
                apply_subscription_import_error(
                    &mut self.subscriptions,
                    &self.pending_commands,
                    message,
                );
            }
        }
        let finished_config_save_id = match &event {
            AppEvent::CommandFinished { id }
                if matches!(
                    self.pending_commands.get(id),
                    Some(AppCommand::SaveConfig { .. })
                ) =>
            {
                Some(*id)
            }
            _ => None,
        };
        let cleared_pending = match &event {
            AppEvent::CommandFinished { id } => {
                let removed = self.pending_commands.remove(id).is_some();
                if let Some(command) = self.pending_after_config_save.remove(id) {
                    self.dispatch_command(command);
                }
                removed
            }
            _ => false,
        };
        let should_refresh_groups = matches!(
            &event,
            AppEvent::SnapshotChanged(_) | AppEvent::RuntimeStatusChanged(_)
        );
        let should_refresh_rules = should_refresh_groups;
        let effect = if self.page_states_suspended_for_tray {
            apply_app_event_to_global_state(&mut self.snapshot, event)
        } else {
            apply_app_event_to_active_state(
                self.active_route,
                &mut self.snapshot,
                &mut self.monitor,
                &mut self.groups,
                &mut self.rules_proxy,
                &mut self.connections,
                &mut self.subscriptions,
                event,
            )
        };
        if !self.page_states_suspended_for_tray {
            // 核心状态或路由变化后重新评估后台订阅，避免隐藏页面继续消费事件。
            self.reconcile_traffic_monitoring();
            self.reconcile_log_monitoring_focus();
            self.reconcile_connections_monitoring_focus();
            if finished_config_save_id.is_some() {
                self.refresh_status_tun_enabled_from_saved_config();
            }
            if should_refresh_groups {
                self.reconcile_proxy_groups_backend();
            }
            if should_refresh_rules {
                self.reconcile_rules_backend();
            }
        }
        if cleared_pending && matches!(effect, ShellAppEventEffect::None) {
            ShellAppEventEffect::Redraw
        } else {
            effect
        }
    }

    pub(super) fn override_debug_pending(&self) -> bool {
        self.pending_commands
            .values()
            .any(|command| matches!(command, AppCommand::DebugOverrideScript { .. }))
    }

    pub(super) fn refresh_status_tun_enabled_from_saved_config(&mut self) {
        if let Some(enabled) = load_saved_tun_enabled(self.command_router.as_ref()) {
            self.status_tun_enabled = enabled;
            self.config_editor.apply_persisted_tun_enable(enabled);
        }
    }

    pub(crate) fn set_monitor_log_filter(&mut self, filter: monitor::LogLevelFilter) {
        self.monitor.set_filter(filter);
    }

    pub(crate) fn clear_monitor_logs(&mut self) {
        self.monitor.clear_logs();
    }
    pub(crate) fn open_connection_detail(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connections.open_detail(id);
        self.notify_connection_result(window, cx);
    }

    pub(crate) fn close_connection_detail(&mut self) {
        self.connections.close_detail();
        self.connection_detail_editor_contents.clear();
    }
    pub(super) fn sync_connection_detail_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.connections_runtime.as_ref() else {
            return;
        };
        let contents = self.connections.detail_json();
        if self.connection_detail_editor_contents == contents {
            return;
        }
        self.connection_detail_editor_contents = contents.clone();
        runtime.detail_editor.update(cx, |input, cx| {
            input.set_value(contents, window, cx);
        });
    }

    pub(super) fn sync_override_preview_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.override_script_runtime.as_ref() else {
            return;
        };
        if !self.override_script.preview_is_open() {
            return;
        }
        let contents = self.override_script.preview_contents();
        if self.override_preview_editor_contents == contents {
            return;
        }
        self.override_preview_editor_contents = contents.to_string();
        runtime.preview_editor.update(cx, |input, cx| {
            input.set_value(contents, window, cx);
        });
    }

    pub(super) fn sync_subscription_yaml_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.subscription_runtime.as_ref() else {
            return;
        };
        let contents = self.subscriptions.yaml_preview_contents();
        if self.subscription_yaml_editor_contents == contents {
            return;
        }
        self.subscription_yaml_editor_contents = contents.clone();
        runtime.inputs.set_yaml_preview(contents, window, cx);
    }

    pub(super) fn sync_active_page_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page_states_suspended_for_tray {
            return;
        }
        // 事件回填只同步当前激活页面的输入控件，避免后台页面被事件重新填充大文本。
        match self.active_route {
            AppRoute::OverrideScript => self.sync_override_preview_editor(window, cx),
            AppRoute::ProxyGroups => {}
            AppRoute::Connections => self.sync_connection_detail_editor(window, cx),
            AppRoute::Subscriptions => self.sync_subscription_yaml_editor(window, cx),
            AppRoute::RulesProxy | AppRoute::Logs | AppRoute::Profiles | AppRoute::Settings => {}
        }
    }

    pub(crate) fn toggle_runtime_rule(&mut self, index: usize, enabled: bool) {
        if let Some(command) = self.rules_proxy.request_rule_enabled(index, enabled) {
            self.dispatch_command(command);
        }
    }

    pub(crate) fn set_override_enabled(&mut self, enabled: bool) {
        let command = self.override_script.set_enabled(enabled);
        self.dispatch_command(command);
    }

    pub(crate) fn debug_override_script(&mut self, cx: &mut Context<Self>) {
        let command = self.override_script.debug();
        self.override_preview_editor_contents.clear();
        self.dispatch_command(command);
        cx.notify();
    }

    pub(crate) fn save_override_script(&mut self, cx: &mut Context<Self>) {
        let command = self.override_script.save();
        self.dispatch_command(command);
        cx.notify();
    }

    pub(crate) fn close_override_preview(&mut self) {
        self.override_script.close_preview();
        self.override_preview_editor_contents.clear();
    }

    pub(crate) fn set_connection_status_filter(
        &mut self,
        status: connections::ConnectionStatusFilter,
    ) {
        self.connections.set_status_filter(status);
    }

    pub(crate) fn set_connection_sort_field(&mut self, field: connections::ConnectionSortField) {
        self.connections.set_sort_field(field);
    }

    pub(crate) fn toggle_connection_sort_direction(&mut self) {
        self.connections.toggle_sort_direction();
    }

    pub(crate) fn request_close_connection(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.connections.request_close_connection(id) {
            self.dispatch_command(command);
        }
        self.notify_connection_result(window, cx);
    }

    pub(crate) fn request_close_all_connections(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connections.request_close_all();
        self.notify_connection_result(window, cx);
    }

    pub(crate) fn cancel_pending_connection_close(&mut self) {
        self.connections.cancel_pending_close();
    }

    pub(crate) fn confirm_pending_connection_close(&mut self) {
        for command in self.connections.confirm_pending_close() {
            self.dispatch_command(command);
        }
    }

    pub(crate) fn select_group(&mut self, name: String) {
        self.groups.select_group(name);
    }

    pub(crate) fn toggle_group_delay_sort(&mut self) {
        self.groups.toggle_delay_sort();
    }

    pub(crate) fn test_selected_group_delay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.groups.test_selected_delay() {
            self.dispatch_command(command);
        }
        self.notify_group_result(window, cx);
    }

    pub(crate) fn close_group_modal(&mut self) {
        self.groups.close_modal();
    }

    pub(crate) fn save_group_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(command) = self.groups.save_form() {
            self.dispatch_command(command);
        }
        self.notify_group_result(window, cx);
    }

    pub(crate) fn select_group_member(
        &mut self,
        group: String,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.groups.select_member(group, member) {
            self.dispatch_command(command);
        }
        self.notify_group_result(window, cx);
    }

    pub(crate) fn test_group_member_delay(
        &mut self,
        group: String,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.groups.test_member_delay(group, member) {
            self.dispatch_command(command);
        }
        self.notify_group_result(window, cx);
    }

    pub(crate) fn set_config_group(&mut self, group: config_editor::ConfigEditorGroup) {
        self.config_editor.set_group(group);
    }

    pub(crate) fn toggle_config_advanced(&mut self, group: config_editor::ConfigEditorGroup) {
        self.config_editor.toggle_advanced(group);
    }

    pub(crate) fn cycle_config_bool(&mut self, field: config_editor::ConfigBoolField) {
        if let Some(command) = self.config_editor.cycle_bool(field) {
            self.dispatch_command(command);
        }
    }

    pub(crate) fn set_config_bool(&mut self, field: config_editor::ConfigBoolField, value: bool) {
        if let Some(command) = self.config_editor.set_bool(field, value) {
            self.dispatch_command(command);
        }
    }

    pub(super) fn toggle_core_from_status_menu(&mut self) {
        match self.snapshot.runtime {
            RuntimeStatus::Running => self.dispatch_command(AppCommand::StopCore),
            RuntimeStatus::Idle | RuntimeStatus::Failed { .. } => {
                self.dispatch_command(AppCommand::StartCore);
            }
            RuntimeStatus::Starting | RuntimeStatus::Stopping => {}
        }
    }

    pub(super) fn toggle_tun_from_status_menu(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 状态栏菜单基于已保存的用户配置生成 SaveConfig，避免配置页旧草稿把 TUN 开关显示或保存错。
        if self.dispatch_saved_tun_toggle(enabled, window, cx) {
            self.status_tun_enabled = enabled;
            self.config_editor.apply_persisted_tun_enable(enabled);
            self.sync_active_page_inputs(window, cx);
        }
    }

    pub(super) fn restart_core_from_status_menu(&mut self) {
        if !matches!(
            self.snapshot.runtime,
            RuntimeStatus::Starting | RuntimeStatus::Stopping
        ) {
            self.dispatch_command(AppCommand::RestartCore);
        }
    }

    pub(super) fn dispatch_saved_tun_toggle(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(router) = self.command_router.as_ref() else {
            super::components::push_global_notice(
                window,
                cx,
                super::components::UiNoticeLevel::Error,
                "后端服务不可用，无法保存 TUN 设置。",
            );
            return false;
        };

        let result: air_error::AppResult<String> = (|| {
            let mut document = router
                .services()
                .current_profile_document()?
                .ok_or_else(|| {
                    ConfigError::Validation("当前核心配置不可用，无法保存 TUN 设置".to_string())
                })?;
            document
                .typed
                .tun
                .get_or_insert_with(TunConfig::default)
                .enable = Some(enabled);
            Ok(ConfigDocument::with_typed(document.typed)?.to_yaml_string()?)
        })();

        match result {
            Ok(profile) => {
                let after_save = if enabled {
                    super::lifecycle::command_after_tun_config_save(&self.snapshot.runtime)
                } else {
                    None
                };
                self.dispatch_command_after_config_save(
                    AppCommand::SaveConfig { profile },
                    after_save,
                );
                true
            }
            Err(error) => {
                tracing::warn!(%error, enabled, "failed to build tun toggle config");
                super::components::push_global_notice(
                    window,
                    cx,
                    super::components::UiNoticeLevel::Error,
                    redact_log_value(&error.to_string()),
                );
                false
            }
        }
    }

    pub(super) fn open_logs_from_status_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(AppRoute::Logs, window, cx);
    }

    pub(crate) fn set_config_text(
        &mut self,
        field: config_editor::ConfigTextField,
        value: impl Into<String>,
    ) {
        let value = value.into();
        if matches!(field, config_editor::ConfigTextField::GlobalMode) {
            // 设置页里的 mode 控件和底部状态栏都表示同一个运行模式；状态栏状态独立保存，
            // 避免配置页关闭后 `ConfigEditorPageState::empty()` 把显示回退到默认值。
            self.status_runtime_mode = status_runtime_mode_value(&value);
        }
        if let Some(command) = self.config_editor.update_text(field, value) {
            self.dispatch_command(command);
        }
    }

    pub(crate) fn set_runtime_mode(
        &mut self,
        mode: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.config_editor.apply_persisted_runtime_mode(mode);
        self.status_runtime_mode = status_runtime_mode_value(mode);
        // 配置页未激活时不创建输入控件；如果已经打开过，则同步当前实体，避免再次进入看到旧值。
        if let Some(runtime) = self.config_runtime.as_ref() {
            runtime
                .inputs
                .global_mode
                .update(cx, |input, cx| input.set_value(mode, window, cx));
        }
        self.dispatch_command(AppCommand::SetRuntimeMode {
            mode: mode.to_string(),
        });
    }

    pub(crate) fn begin_edit_subscription_by_id(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = self.subscriptions.begin_edit_by_id(&id) {
            self.subscription_yaml_editor_contents.clear();
            if let Some(runtime) = self.subscription_runtime.as_ref() {
                runtime.inputs.set_from_form(&form, window, cx);
            }
            self.load_subscription_yaml_preview(form.id, window, cx);
        }
        self.notify_subscription_result(window, cx);
    }

    pub(crate) fn close_subscription_modal(&mut self) {
        self.subscriptions.close_modal();
        self.subscription_yaml_editor_contents.clear();
    }

    pub(crate) fn save_subscription_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(command) = self.subscriptions.save_form() {
            self.dispatch_command(command);
        }
        self.notify_subscription_result(window, cx);
    }

    pub(crate) fn save_subscription_config_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.subscriptions.save_config_form();
        self.notify_subscription_result(window, cx);
    }

    pub(crate) fn select_subscription(&mut self, id: String) {
        if let Some(command) = self.subscriptions.select(id) {
            self.dispatch_command(command);
        }
    }

    pub(crate) fn delete_subscription_by_id(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.subscriptions.delete_by_id(id) {
            self.dispatch_command(command);
        }
        self.notify_subscription_result(window, cx);
    }

    pub(crate) fn update_subscription_by_id(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.subscriptions.update_by_id(id) {
            self.dispatch_command(command);
        }
        self.notify_subscription_result(window, cx);
    }

    pub(crate) fn reorder_subscription_before(
        &mut self,
        dragged_id: String,
        target_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.subscriptions.reorder_before(&dragged_id, &target_id) {
            self.dispatch_command(command);
        }
        self.notify_subscription_result(window, cx);
    }

    pub(super) fn load_subscription_yaml_preview(
        &mut self,
        subscription_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.subscription_yaml_editor_contents.clear();
        if let Some(runtime) = self.subscription_runtime.as_ref() {
            runtime
                .inputs
                .set_yaml_preview("# 姝ｅ湪璇诲彇璁㈤槄缂撳瓨\n", window, cx);
        }
        self.dispatch_command(AppCommand::LoadSubscriptionYaml { subscription_id });
    }

    pub(crate) fn import_subscription_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(command) = self.subscriptions.import_url() {
            if let Some(runtime) = self.subscription_runtime.as_ref() {
                runtime.inputs.clear_import_url(window, cx);
            }
            self.dispatch_command(command);
        }
        self.notify_subscription_result(window, cx);
    }

    pub(crate) fn choose_subscription_yaml_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("閫夋嫨 YAML 璁㈤槄閰嶇疆".into()),
        });
        let shell = cx.entity().clone();
        cx.spawn_in(window, async move |_, window| {
            let Some(Ok(paths)) = paths.await.ok() else {
                return;
            };
            let Some(path) = paths.into_iter().flatten().next() else {
                return;
            };
            let _ = window.update(move |window, cx| {
                let _ = shell.update(cx, |shell, cx| {
                    if let Some(command) = shell.subscriptions.import_yaml_file(path) {
                        shell.dispatch_command(command);
                    }
                    shell.notify_subscription_result(window, cx);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    pub(super) fn notify_subscription_result(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(notice) = self.subscriptions.take_notice() else {
            return;
        };
        super::components::push_global_notice(
            window,
            cx,
            subscription_ui_notice_level(notice.level),
            notice.message,
        );
    }

    pub(super) fn notify_connection_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(notice) = self.connections.take_notice() else {
            return;
        };
        let level = match notice.level {
            connections::ConnectionNoticeLevel::Info => super::components::UiNoticeLevel::Info,
            connections::ConnectionNoticeLevel::Error => super::components::UiNoticeLevel::Error,
        };
        super::components::push_global_notice(window, cx, level, notice.message);
    }

    pub(crate) fn notify_connection_success(
        &self,
        message: impl Into<gpui::SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        super::components::push_global_notice(
            window,
            cx,
            super::components::UiNoticeLevel::Success,
            message,
        );
    }

    pub(super) fn notify_group_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(notice) = self.groups.take_notice() else {
            return;
        };
        let level = match notice.level {
            proxy_groups::GroupNoticeLevel::Success => super::components::UiNoticeLevel::Success,
            proxy_groups::GroupNoticeLevel::Warning => super::components::UiNoticeLevel::Warning,
            proxy_groups::GroupNoticeLevel::Error => super::components::UiNoticeLevel::Error,
        };
        super::components::push_global_notice(window, cx, level, notice.message);
    }

    pub(super) fn notify_subscription_diagnostics(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.page_states_suspended_for_tray || self.active_route != AppRoute::Subscriptions {
            return;
        }

        for (level, message) in collect_subscription_diagnostic_notices(
            &self.subscriptions,
            &mut self.subscription_diagnostic_notices,
        ) {
            super::components::push_global_notice(window, cx, level, message);
        }
    }

    pub(crate) fn set_settings_theme(
        &mut self,
        theme: GuiThemePreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preference = ShellThemePreference::from(theme);
        self.theme_preference = preference;
        self.settings.set_theme(theme);
        apply_theme_preference(preference, self.system_theme_mode, window, cx);
        self.persist_settings();
    }

    pub(crate) fn save_config_group(
        &mut self,
        group: config_editor::ConfigEditorGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = self.config_editor.save_group(group) {
            if group == config_editor::ConfigEditorGroup::Tun {
                self.status_tun_enabled = self
                    .config_editor
                    .view_model()
                    .draft
                    .tun
                    .enable
                    .unwrap_or(false);
            }
            self.dispatch_command(command);
        }
        self.notify_config_result(window, cx);
        cx.notify();
    }

    pub(crate) fn set_settings_bool(&mut self, field: settings::SettingsBoolField, value: bool) {
        self.settings.set_bool(field, value);
        self.persist_settings();
        if field == settings::SettingsBoolField::Autostart {
            sync_platform_autostart(value);
        }
    }

    pub(crate) fn set_settings_text(
        &mut self,
        field: settings::SettingsTextField,
        value: impl Into<String>,
    ) {
        self.settings.set_text(field, value);
        self.persist_settings();
    }

    pub(crate) fn request_core_service_toggle(&mut self, install: bool) {
        self.core_service_confirmation = Some(if install {
            CoreServiceConfirmation::Install
        } else {
            CoreServiceConfirmation::Uninstall
        });
    }

    pub(super) fn cancel_core_service_confirmation(&mut self) {
        self.core_service_confirmation = None;
    }

    pub(super) fn confirm_core_service_toggle(&mut self) {
        let Some(action) = self.core_service_confirmation.take() else {
            return;
        };
        match action {
            CoreServiceConfirmation::Install => {
                self.dispatch_command(AppCommand::InstallCoreService);
            }
            CoreServiceConfirmation::Uninstall => {
                self.dispatch_command(AppCommand::UninstallCoreService);
            }
        }
    }

    pub(super) fn notify_config_result(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(notice) = self.config_editor.view_model().notice else {
            return;
        };
        // 配置保存成功需要以后端命令的落盘/重载结果为准；页面本地 success 只表示命令已提交，
        // 不再推全局 toast，避免和 `SaveConfig` 完成通知重复。
        if !should_push_config_notice(notice.level) {
            return;
        }
        let level = match notice.level {
            config_editor::ConfigNoticeLevel::Success => super::components::UiNoticeLevel::Success,
            config_editor::ConfigNoticeLevel::Warning => super::components::UiNoticeLevel::Warning,
            config_editor::ConfigNoticeLevel::Error => super::components::UiNoticeLevel::Error,
        };
        super::components::push_global_notice(window, cx, level, notice.message);
    }

    pub(super) fn sync_config_save_notices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty_groups = self.config_editor.view_model().dirty_groups;

        for group in dirty_groups.iter().copied() {
            if self.config_save_notices.insert(group) {
                self.push_config_save_notice(group, window, cx);
            }
        }

        let stale_groups = self
            .config_save_notices
            .iter()
            .copied()
            .filter(|group| !dirty_groups.contains(group))
            .collect::<Vec<_>>();
        for group in stale_groups {
            self.config_save_notices.remove(&group);
            super::components::remove_persistent_global_notice(
                window,
                cx,
                config_save_notice_key(group),
            );
        }
    }

    pub(super) fn push_config_save_notice(
        &self,
        group: config_editor::ConfigEditorGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shell = cx.entity().clone();
        super::components::push_persistent_global_notice(
            window,
            cx,
            super::components::UiNoticeLevel::Warning,
            config_save_notice_key(group),
            format!("{} 已修改，需要保存后生效。", group.title()),
            "保存",
            move |window, cx| {
                let _ = shell.update(cx, |shell, cx| {
                    shell.save_config_group(group, window, cx);
                });
            },
        );
    }

    pub(super) fn persist_settings(&mut self) {
        let Some(router) = &self.command_router else {
            tracing::warn!("app services unavailable, skip persisting gui settings");
            return;
        };
        if let Err(error) = router.services().save_settings(self.settings.settings()) {
            tracing::warn!(%error, "failed to persist gui settings");
        }
    }
}

pub(super) fn collect_subscription_diagnostic_notices(
    subscriptions: &subscriptions::SubscriptionPageState,
    seen: &mut BTreeSet<String>,
) -> Vec<(super::components::UiNoticeLevel, String)> {
    let mut notices = Vec::new();
    let view_model = subscriptions.view_model();

    for item in &view_model.items {
        let checked_at = item
            .last_checked_at
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string());
        let mut emitted_messages = BTreeSet::new();

        if let Some(error) = item.last_error.as_ref() {
            let key = format!("last-error:{}:{checked_at}:{error}", item.id);
            if seen.insert(key) && emitted_messages.insert(error.clone()) {
                notices.push((
                    super::components::UiNoticeLevel::Warning,
                    format!("{}: {}", item.name, error),
                ));
            }
        }

        for diagnostic in &item.diagnostics {
            let key = format!(
                "diagnostic:{}:{}:{}:{}",
                item.id, checked_at, diagnostic.code, diagnostic.message
            );
            if seen.insert(key) && emitted_messages.insert(diagnostic.message.clone()) {
                notices.push((
                    subscription_diagnostic_notice_level(diagnostic.severity.clone()),
                    format!("{}: {}", item.name, diagnostic.message),
                ));
            }
        }
    }

    notices
}
