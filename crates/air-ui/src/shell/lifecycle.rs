use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

static APP_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn launch(force_start_core: bool, single_instance_events: Receiver<SingleInstanceEvent>) {
    gpui_platform::application()
        .with_assets(icons::AppAssets::new())
        .run(move |cx: &mut App| {
            reset_app_quit_request();
            gpui_component::init(cx);
            Theme::global_mut(cx).font_family = super::render::app_ui_font_family().into();
            super::components::enforce_visible_scrollbars(cx);
            super::components::configure_global_notifications(cx);
            configure_app_menu(cx);

            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(
                    size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
                    cx,
                )),
                titlebar: Some(main_window_titlebar_options()),
                window_decorations: Some(gpui::WindowDecorations::Client),
                window_min_size: Some(size(px(860.0), px(560.0))),
                app_id: Some("air".to_string()),
                ..Default::default()
            };

            cx.spawn(async move |cx| {
                cx.open_window(window_options, |window, cx| {
                    let shell = cx
                        .new(|cx| Shell::new(window, cx, force_start_core, single_instance_events));
                    let shutdown_subscription = shell.update(cx, |_, cx| {
                        cx.on_app_quit(|shell, _| {
                            shell.request_app_quit();
                            shell.stop_core_before_app_exit();
                            async {}
                        })
                    });
                    shell.update(cx, |shell, _| {
                        shell._shutdown_subscription = Some(shutdown_subscription);
                    });
                    let close_shell = shell.clone();
                    window.on_window_should_close(cx, move |window, cx| {
                        if close_shell.read(cx).should_close_window_for_intent() {
                            close_shell.read(cx).stop_core_before_app_exit();
                            return true;
                        }
                        let behavior = close_shell
                            .read(cx)
                            .settings
                            .settings()
                            .close_window_behavior;
                        if behavior == CloseWindowBehavior::Exit {
                            let _ = close_shell.update(cx, |shell, _| {
                                shell.request_app_quit();
                            });
                            close_shell.read(cx).stop_core_before_app_exit();
                            cx.quit();
                            return false;
                        }
                        if behavior == CloseWindowBehavior::Tray {
                            let _ = close_shell.update(cx, |shell, cx| {
                                shell.hide_window_from_tray(window, cx);
                            });
                            return false;
                        }
                        false
                    });
                    // gpui-component 的 Root 必须作为窗口第一层，用来承载通知、弹窗和焦点管理。
                    cx.new(|cx| Root::new(shell, window, cx))
                })
                .expect("failed to open main window");
            })
            .detach();

            cx.activate(true);
        });
}

pub(super) fn main_window_titlebar_options() -> gpui::TitlebarOptions {
    let mut options = TitleBar::title_bar_options();
    options.title = Some("Air".into());
    options
}

pub(super) fn configure_app_menu(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| {
        mark_app_quit_requested();
        cx.quit();
    });
    cx.bind_keys([app_quit_key_binding()]);
    cx.set_menus(app_menus());
}

pub(super) fn app_quit_key_binding() -> KeyBinding {
    KeyBinding::new("cmd-q", Quit, None)
}

pub(super) fn app_menus() -> Vec<Menu> {
    vec![Menu::new("Air").items([
        MenuItem::os_submenu("Services", SystemMenuType::Services),
        MenuItem::separator(),
        MenuItem::action("退出 Air", Quit),
    ])]
}

pub(super) fn create_tray() -> (TrayHandle, Receiver<TrayEvent>) {
    let options = TrayOptions {
        tooltip: "Air mihomo 管理器".to_string(),
        icon_png: Some(icons::brand_icon_png_bytes()),
    };
    match air_platform::tray::start_tray(options) {
        Ok((handle, events)) => {
            if handle.is_supported() {
                tracing::info!("system tray initialized");
            } else {
                tracing::info!(
                    "system tray unsupported on current platform; continuing without tray"
                );
            }
            (handle, events)
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to initialize system tray");
            let (_sender, receiver) = mpsc::channel();
            (TrayHandle::disabled(), receiver)
        }
    }
}

pub(super) fn spawn_tray_event_loop(
    receiver: Receiver<TrayEvent>,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    let shell = cx.entity().clone();
    cx.spawn_in(window, async move |_, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            loop {
                match receiver.try_recv() {
                    Ok(event) => {
                        let updated = cx.update({
                            let shell = shell.clone();
                            move |window, cx| {
                                let _ = shell.update(cx, |shell, cx| {
                                    shell.handle_tray_event(event, window, cx);
                                });
                            }
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
        }
    })
    .detach();
}

pub(super) fn spawn_single_instance_event_loop(
    receiver: Receiver<SingleInstanceEvent>,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    let shell = cx.entity().clone();
    cx.spawn_in(window, async move |_, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            loop {
                match receiver.try_recv() {
                    Ok(SingleInstanceEvent::ShowWindow) => {
                        let updated = cx.update({
                            let shell = shell.clone();
                            move |window, cx| {
                                let _ = shell.update(cx, |shell, cx| {
                                    tracing::info!(
                                        "single instance requested existing window restore"
                                    );
                                    shell.show_window_from_single_instance(window, cx);
                                });
                            }
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
        }
    })
    .detach();
}

pub(super) fn spawn_initial_window_hide(window: &mut Window, cx: &mut Context<Shell>) {
    // Windows 窗口需要等 GPUI 完成首次显示后再隐藏，否则 HWND 可能尚未进入可恢复状态。
    cx.spawn_in(window, async move |_, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
        let _ = cx.update(|window, _| {
            if let Err(error) = air_platform::window::hide_window(window) {
                tracing::warn!(%error, "failed to hide main window on silent startup");
            }
        });
    })
    .detach();
}

pub(super) fn spawn_tray_resource_cleanup(
    window: &mut Window,
    cx: &mut Context<Shell>,
    generation: u64,
) {
    let shell = cx.entity().clone();
    cx.spawn_in(window, async move |_, cx| {
        cx.background_executor()
            .timer(TRAY_RESOURCE_RELEASE_DELAY)
            .await;
        let _ = shell.update(cx, |shell, cx| {
            if shell.page_states_suspended_for_tray && shell.tray_cleanup_generation == generation {
                shell.destroy_all_page_states_for_tray();
                air_telemetry::memory::shrink_process_memory("tray-hide-delayed");
                cx.notify();
            }
        });
    })
    .detach();
}

pub(super) fn spawn_subscription_refresh_loop(cx: &mut Context<Shell>) {
    cx.spawn(async move |shell, cx| {
        loop {
            cx.background_executor()
                .timer(SUBSCRIPTION_REFRESH_INTERVAL)
                .await;
            let should_continue = shell
                .update(cx, |shell, cx| {
                    if shell.page_states_suspended_for_tray {
                        return true;
                    }
                    // 定时器只派发检查命令；到期判断和远程更新在 app/controller 层完成。
                    if should_dispatch_subscription_refresh(&shell.pending_commands)
                        && shell.command_router.is_some()
                    {
                        shell.dispatch_command(AppCommand::RefreshDueSubscriptions);
                        cx.notify();
                    }
                    true
                })
                .unwrap_or(false);
            if !should_continue {
                break;
            }
        }
    })
    .detach();
}

pub(super) fn should_dispatch_subscription_refresh(
    pending_commands: &BTreeMap<CommandId, AppCommand>,
) -> bool {
    !pending_commands.values().any(|command| {
        matches!(
            command,
            AppCommand::RefreshDueSubscriptions | AppCommand::UpdateSubscription { .. }
        )
    })
}

pub(super) fn should_run_connections_monitoring(
    active_route: AppRoute,
    page_states_suspended_for_tray: bool,
    runtime: &RuntimeStatus,
) -> bool {
    !page_states_suspended_for_tray
        && active_route == AppRoute::Connections
        && matches!(runtime, RuntimeStatus::Running)
}

pub(super) fn should_run_traffic_monitoring(
    _active_route: AppRoute,
    page_states_suspended_for_tray: bool,
    runtime: &RuntimeStatus,
) -> bool {
    // 状态栏网速是跨页面信息，只跟随核心运行状态和托盘挂起状态，不能被当前路由限制。
    !page_states_suspended_for_tray && matches!(runtime, RuntimeStatus::Running)
}

pub(super) fn should_run_log_monitoring(
    active_route: AppRoute,
    page_states_suspended_for_tray: bool,
) -> bool {
    !page_states_suspended_for_tray && active_route == AppRoute::Logs
}

pub(super) fn load_app_backing() -> (
    AppSettings,
    Option<AppCommandRouter>,
    AppSnapshot,
    Option<AppStateStore>,
) {
    match AppServices::new() {
        Ok(services) => {
            let settings = services.load_settings().unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to load gui settings, using defaults");
                AppSettings::default()
            });
            let snapshot = services.snapshots.snapshot();
            let snapshot_store = services.snapshots.clone();
            (
                settings,
                Some(AppCommandRouter::new(services)),
                snapshot,
                Some(snapshot_store),
            )
        }
        Err(error) => {
            tracing::warn!(%error, "failed to initialize app services, using default gui settings");
            (AppSettings::default(), None, AppSnapshot::default(), None)
        }
    }
}

pub(super) fn dispatch_startup_prepare(router: Option<&AppCommandRouter>, start_core: bool) {
    if let Some(router) = router {
        // 启动时自动完成核心检测和准备；失败通过 app event 写入快照和通知，不阻塞窗口创建。
        router.dispatch(if start_core {
            AppCommand::StartCore
        } else {
            AppCommand::PrepareCore
        });
        // 订阅定时更新在启动后立即检查一次，之后由 GPUI 定时器周期性派发同一命令。
        router.dispatch(AppCommand::RefreshDueSubscriptions);
    }
}

pub(super) fn should_start_core_on_startup(settings: &AppSettings, force_start_core: bool) -> bool {
    // UAC 提权后的新实例必须继续完成用户刚触发的启动动作。
    force_start_core || settings.start_core_after_launch
}

pub(super) fn should_close_window_for_intent(
    settings: &AppSettings,
    app_quit_requested: bool,
) -> bool {
    app_quit_requested || settings.close_window_behavior != CloseWindowBehavior::Tray
}

pub(super) fn command_after_tun_config_save(runtime: &RuntimeStatus) -> Option<AppCommand> {
    match runtime {
        RuntimeStatus::Running => Some(AppCommand::RestartCore),
        RuntimeStatus::Idle | RuntimeStatus::Failed { .. } => Some(AppCommand::StartCore),
        RuntimeStatus::Starting | RuntimeStatus::Stopping => None,
    }
}

pub(super) fn reset_app_quit_request() {
    APP_QUIT_REQUESTED.store(false, Ordering::SeqCst);
}

pub(super) fn mark_app_quit_requested() {
    APP_QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

pub(super) fn app_quit_requested() -> bool {
    APP_QUIT_REQUESTED.load(Ordering::SeqCst)
}

pub(super) fn should_hide_window_on_startup(settings: &AppSettings, tray_supported: bool) -> bool {
    settings.silent_start && tray_supported
}
