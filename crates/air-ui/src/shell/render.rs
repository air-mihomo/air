use super::*;

impl Shell {
    pub(super) fn active_page_state(&self) -> PageState {
        PageState::Empty {
            message: format!(
                "{}页面将在后续任务中接入真实数据。",
                self.active_route.descriptor().label
            ),
        }
    }

    pub(super) fn render_title_bar_menu_item(
        &self,
        route: AppRoute,
        palette: ShellPalette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = route == self.active_route;
        let descriptor = route.descriptor();
        let text_color = if active {
            palette.active_text
        } else {
            palette.text
        };

        div()
            .id(format!("title-menu-{}", route.id()))
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(24.0))
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .text_color(text_color)
            .bg(if active {
                palette.active
            } else {
                palette.background
            })
            .hover(move |this| {
                if active {
                    this.bg(palette.active_hover)
                } else {
                    this.bg(palette.hover)
                }
            })
            .child(icons::icon(descriptor.icon, text_color))
            .child(descriptor.label)
            // 菜单项位于 gpui-component TitleBar 内部；按下事件必须截断，
            // 否则外层标题栏会把点击识别为窗口拖拽区域，导致 on_click 不触发。
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.navigate(route, window, cx);
                cx.notify();
            }))
    }

    pub(super) fn render_page(
        &mut self,
        palette: ShellPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.active_route == AppRoute::Logs {
            if self.log_runtime.is_none() {
                self.log_runtime = Some(create_log_runtime(window, cx));
            }
            let runtime = self
                .log_runtime
                .as_ref()
                .expect("log runtime should be available after lazy initialization");
            return div()
                .id("page-logs-root")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .overflow_hidden()
                .bg(palette.background)
                .child(monitor::render_log_page(
                    &self.monitor,
                    runtime.monitor_search_input.clone(),
                    self.monitor_log_scroll_handle.clone(),
                    palette,
                    cx,
                ));
        }

        if self.active_route == AppRoute::RulesProxy {
            if self.rules_proxy_runtime.is_none() {
                self.rules_proxy_runtime = Some(create_rules_proxy_runtime(window, cx));
            }
            let runtime = self
                .rules_proxy_runtime
                .as_ref()
                .expect("rules proxy runtime should be available after lazy initialization");
            return div()
                .id("page-scroll-rules-proxy")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .h_full()
                .overflow_hidden()
                .bg(palette.background)
                .child(rules::render_rules_proxy_page(
                    &self.rules_proxy,
                    rules::RulesProxyPageInputs {
                        search: runtime.search_input.clone(),
                    },
                    self.rules_proxy_scroll_handle.clone(),
                    palette,
                    cx,
                ));
        }

        if self.active_route == AppRoute::OverrideScript {
            if self.override_script_runtime.is_none() {
                let source = load_initial_override_script(self.command_router.as_ref());
                self.override_script = override_script::OverrideScriptPageState::new(
                    self.settings.settings().override_script_enabled,
                    source.clone(),
                );
                self.override_script_runtime =
                    Some(create_override_script_runtime(source, window, cx));
            }
            let runtime = self
                .override_script_runtime
                .as_ref()
                .expect("override script runtime should be available after lazy initialization");
            let debug_pending = self
                .pending_commands
                .values()
                .any(|command| matches!(command, AppCommand::DebugOverrideScript { .. }));
            let save_pending = self.pending_commands.values().any(|command| {
                matches!(
                    command,
                    AppCommand::SaveOverrideScript { .. }
                        | AppCommand::SetOverrideScriptEnabled { .. }
                )
            });
            return div()
                .id("page-scroll-override-script")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .h_full()
                .overflow_hidden()
                .bg(palette.background)
                .child(override_script::render_override_script_page(
                    &self.override_script,
                    override_script::OverrideScriptPageInputs {
                        editor: runtime.editor.clone(),
                        preview_editor: runtime.preview_editor.clone(),
                    },
                    debug_pending,
                    save_pending,
                    palette,
                    cx,
                ));
        }

        if self.active_route == AppRoute::ProxyGroups {
            if self.group_runtime.is_none() {
                self.group_runtime = Some(create_group_runtime(&self.groups, window, cx));
            }
            let runtime = self
                .group_runtime
                .as_ref()
                .expect("group runtime should be available after lazy initialization");
            return div()
                .id("page-scroll-proxy-groups")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .overflow_hidden()
                .bg(palette.background)
                .child(proxy_groups::render_groups_page(
                    &self.groups,
                    proxy_groups::GroupPageInputs {
                        search: runtime.search_input.clone(),
                        group_scroll_handle: runtime.group_scroll_handle.clone(),
                        member_scroll_handle: runtime.member_scroll_handle.clone(),
                        proxies: runtime.proxies_input.clone(),
                        providers: runtime.providers_input.clone(),
                        filter: runtime.filter_input.clone(),
                        exclude_filter: runtime.exclude_filter_input.clone(),
                    },
                    palette,
                    window.viewport_size().width.as_f32().max(0.0),
                    cx,
                ));
        }

        if self.active_route == AppRoute::Connections {
            if self.connections_runtime.is_none() {
                self.connections_runtime = Some(create_connections_runtime(window, cx));
            }
            let runtime = self
                .connections_runtime
                .as_ref()
                .expect("connections runtime should be available after lazy initialization");
            return div()
                .id("page-scroll-connections")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .overflow_hidden()
                .bg(palette.background)
                .child(connections::render_connections_page(
                    &self.connections,
                    runtime.inputs.clone(),
                    runtime.detail_editor.clone(),
                    palette,
                    cx,
                ));
        }

        if self.active_route == AppRoute::Profiles {
            if self.config_runtime.is_none() {
                self.config_editor = load_initial_config_editor(self.command_router.as_ref());
                self.config_runtime = Some(create_config_runtime(&self.config_editor, window, cx));
            }
            let runtime = self
                .config_runtime
                .as_ref()
                .expect("config runtime should be available after lazy initialization");
            return div()
                .id("page-scroll-config-editor")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .overflow_hidden()
                .p_5()
                .bg(palette.background)
                .child(config_editor::render_config_editor_page(
                    &self.config_editor,
                    runtime.inputs.clone(),
                    palette,
                    cx,
                ));
        }

        if self.active_route == AppRoute::Subscriptions {
            if self.subscription_runtime.is_none() {
                if self.subscriptions.view_model().items.is_empty() {
                    self.subscriptions = load_initial_subscriptions(self.command_router.as_ref());
                }
                self.subscription_runtime =
                    Some(create_subscription_runtime(&self.subscriptions, window, cx));
            }
            let runtime = self
                .subscription_runtime
                .as_ref()
                .expect("subscription runtime should be available after lazy initialization");
            return div()
                .id("page-scroll-subscriptions")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .h_full()
                .overflow_hidden()
                .bg(palette.background)
                .child(subscriptions::render_subscription_page(
                    &self.subscriptions,
                    runtime.inputs.clone(),
                    palette,
                    window.viewport_size().width.as_f32().max(0.0),
                    cx,
                ));
        }

        if self.active_route == AppRoute::Settings {
            if self.config_runtime.is_none() {
                self.config_editor = load_initial_config_editor(self.command_router.as_ref());
                self.config_runtime = Some(create_config_runtime(&self.config_editor, window, cx));
            }
            self.sync_config_save_notices(window, cx);
            let runtime = self
                .config_runtime
                .as_ref()
                .expect("settings config runtime should be available after lazy initialization");
            return div()
                .id("page-scroll-settings")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.0))
                .overflow_hidden()
                .p_5()
                .bg(palette.background)
                .child(settings::render_settings_page(
                    &self.settings,
                    self.settings_inputs.clone(),
                    &self.config_editor,
                    runtime.inputs.clone(),
                    self.snapshot.core_service,
                    palette,
                    cx,
                ));
        }

        div()
            .id("page-scroll-placeholder")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .p_5()
            .bg(palette.background)
            .child(render_page_state(self.active_page_state(), palette))
    }

    pub(super) fn pending_core_command(&self) -> Option<&AppCommand> {
        self.pending_commands.values().find(|command| {
            matches!(
                command,
                AppCommand::StartCore | AppCommand::StopCore | AppCommand::RestartCore
            )
        })
    }

    pub(super) fn render_core_service_confirmation(
        &self,
        palette: ShellPalette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(action) = self.core_service_confirmation else {
            return div();
        };
        let (title, message, confirm) = match action {
            CoreServiceConfirmation::Install => (
                "安装内核服务",
                "将通过 UAC 创建 Windows Service，后续 TUN 内核可由服务启动。",
                "安装",
            ),
            CoreServiceConfirmation::Uninstall => (
                "卸载内核服务",
                "将通过 UAC 停止并删除 Windows Service，之后 TUN 内核启动会要求重新安装服务。",
                "卸载",
            ),
        };

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .bg(palette.background.alpha(0.96))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w(px(420.0))
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.page)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_lg()
                            .font_bold()
                            .child(icons::icon(Icon::BadgeInfo, palette.active))
                            .child(title),
                    )
                    .child(div().text_sm().text_color(palette.muted).child(message))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(core_service_modal_button(
                                "core-service-cancel",
                                "取消",
                                false,
                                palette,
                                cx,
                                |shell, cx| {
                                    shell.cancel_core_service_confirmation();
                                    cx.notify();
                                },
                            ))
                            .child(core_service_modal_button(
                                "core-service-confirm",
                                confirm,
                                true,
                                palette,
                                cx,
                                |shell, cx| {
                                    shell.confirm_core_service_toggle();
                                    cx.notify();
                                },
                            )),
                    ),
            )
    }

    pub(super) fn render_status_bar(
        &self,
        palette: ShellPalette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tun_enabled = self.status_tun_enabled;
        let core_pending = self.pending_core_command().is_some();
        let tun_pending = self
            .pending_commands
            .values()
            .any(|command| matches!(command, AppCommand::SaveConfig { .. }));

        div()
            .flex()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .px_4()
            .h(px(36.0))
            .text_xs()
            .text_color(palette.muted)
            .bg(palette.background)
            .border_1()
            .border_color(palette.border)
            // 状态栏属于全局外壳，放在根布局底部才能横跨完整页面区。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(status_bar_core_item(
                        &self.snapshot,
                        tun_enabled,
                        core_pending,
                        tun_pending,
                        palette,
                        cx,
                    ))
                    .child(status_bar_mode_switch(
                        &self.status_runtime_mode,
                        palette,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(traffic_item(
                        Icon::ArrowUp,
                        &self.monitor.upload_text(),
                        palette,
                    ))
                    .child(traffic_item(
                        Icon::ArrowDown,
                        &self.monitor.download_text(),
                        palette,
                    )),
            )
    }

    pub(super) fn render_title_bar(
        &self,
        palette: ShellPalette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut nav = div()
            .flex()
            .items_center()
            .justify_center()
            .gap_1()
            .h_full()
            .min_w_0();
        for route in AppRoute::all() {
            nav = nav.child(self.render_title_bar_menu_item(*route, palette, cx));
        }

        TitleBar::new().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .min_w_0()
                .w_full()
                .h_full()
                .pr_2()
                .text_color(palette.text)
                // 自定义标题栏中间承载全局导航；路由切换仍只触发 Shell 的页面状态装卸。
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .w(px(TITLE_BAR_SIDE_WIDTH))
                        .min_w(px(TITLE_BAR_SIDE_WIDTH))
                        .child(
                            img(icons::brand_titlebar_icon_asset_path())
                                .w(px(22.0))
                                .h(px(22.0))
                                .object_fit(ObjectFit::Contain),
                        )
                        .child(div().text_sm().font_semibold().child("Air")),
                )
                .child(div().flex_1().min_w_0().flex().justify_center().child(nav))
                .child(
                    div()
                        .w(px(TITLE_BAR_SIDE_WIDTH))
                        .min_w(px(TITLE_BAR_SIDE_WIDTH)),
                ),
        )
    }
}
impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self
            .theme_preference
            .resolved_mode(self.system_theme_mode)
            .palette();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(palette.background)
            .text_color(palette.text)
            .font(app_ui_font())
            .child(self.render_title_bar(palette, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(self.render_page(palette, window, cx)),
                    ),
            )
            .child(self.render_core_service_confirmation(palette, cx))
            .child(self.render_status_bar(palette, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

pub(super) fn app_ui_font() -> gpui::Font {
    font(app_ui_font_family())
}

pub(super) fn app_ui_font_family() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica"
    } else {
        ".SystemUIFont"
    }
}

pub(super) fn core_service_modal_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
    palette: ShellPalette,
    cx: &mut Context<Shell>,
    listener: impl Fn(&mut Shell, &mut Context<Shell>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(32.0))
        .px_3()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .font_bold()
        .bg(if primary {
            palette.active
        } else {
            palette.subtle
        })
        .text_color(if primary {
            palette.active_text
        } else {
            palette.text
        })
        .hover(move |this| {
            if primary {
                this.bg(palette.active_hover)
            } else {
                this.bg(palette.hover)
            }
        })
        .child(label)
        .on_click(cx.listener(move |shell, _, _, cx| listener(shell, cx)))
}

pub(super) fn render_page_state(state: PageState, palette: ShellPalette) -> impl IntoElement {
    let (title, message, icon) = match state {
        PageState::Loading => (
            "加载中",
            "正在读取页面数据。".to_string(),
            Icon::CircleDashed,
        ),
        PageState::Error { message } => ("出现错误", message, Icon::AlertCircle),
        PageState::Empty { message } => ("暂无内容", message, Icon::Circle),
        PageState::Ready => (
            "准备就绪",
            "页面容器已挂载。".to_string(),
            Icon::CheckCircle,
        ),
    };

    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .size_full()
        .min_h(px(360.0))
        .rounded_md()
        .border_1()
        .border_color(palette.border)
        .bg(palette.page)
        .child(icons::icon(icon, palette.active))
        .child(
            div()
                .text_lg()
                .font_bold()
                .text_color(palette.text)
                .child(title),
        )
        .child(div().text_sm().text_color(palette.muted).child(message))
}
