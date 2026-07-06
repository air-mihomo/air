use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, Hsla, InteractiveElement, IntoElement, KeyBinding, Menu,
    MenuItem, MouseButton, ObjectFit, ParentElement, PathPromptOptions, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, StyledImage, Subscription, SystemMenuType, Window,
    WindowAppearance, WindowBounds, WindowOptions, actions, div, font, img, px, rgb, size,
};
use gpui_component::input::{InputEvent, InputState, TabSize};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Root, StyledExt, Theme, ThemeMode, TitleBar, VirtualListScrollHandle};

use air_app::{
    AppCommand, AppCommandRouter, AppEvent, AppNotificationLevel, AppServices, AppSnapshot,
    AppStateStore, CommandId, RuntimeStatus,
};
use air_config::{ConfigDocument, model::TunConfig};
use air_error::ConfigError;
use air_mihomo::streams::StreamEvent;
use air_mihomo::subscriptions::SubscriptionDiagnosticSeverity;
use air_platform::single_instance::SingleInstanceEvent;
use air_platform::tray::{TrayEvent, TrayHandle, TrayOptions};
use air_settings::{AppSettings, CloseWindowBehavior, GuiThemePreference};
use air_telemetry::redaction::redact_log_value;

use super::{
    components,
    icons::{self, Icon},
    pages::{
        config_editor, connections, monitor, override_script, proxy_groups, rules, settings,
        subscriptions,
    },
    routes::AppRoute,
};

const WINDOW_WIDTH: f32 = 1080.0;
const WINDOW_HEIGHT: f32 = 720.0;
const TITLE_BAR_SIDE_WIDTH: f32 = 160.0;
const SUBSCRIPTION_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const TRAY_RESOURCE_RELEASE_DELAY: Duration = Duration::from_secs(3);
const CODE_EDITOR_TAB_SIZE: usize = 4;

actions!(air, [Quit]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageState {
    Loading,
    Error { message: String },
    Empty { message: String },
    Ready,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShellAppEventEffect {
    None,
    Redraw,
    UserVisibleError(String),
    UserNotification(AppNotificationLevel, String),
}

impl ShellAppEventEffect {
    fn should_notify(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellThemeMode {
    Light,
    Dark,
}

impl ShellThemeMode {
    fn as_component_mode(self) -> ThemeMode {
        match self {
            ShellThemeMode::Light => ThemeMode::Light,
            ShellThemeMode::Dark => ThemeMode::Dark,
        }
    }

    fn from_window_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
        }
    }

    fn palette(self) -> ShellPalette {
        match self {
            ShellThemeMode::Light => ShellPalette {
                background: c(0xf5f5f5),
                surface: c(0xfbfbf8),
                page: c(0xf5f5f5),
                border: c(0xd8ddd2),
                text: c(0x17201a),
                muted: c(0x667064),
                subtle: c(0xe7ebe1),
                hover: c(0xdfe5d8),
                active: c(0x3fa0fe),
                active_hover: c(0x238cf0),
                active_text: c(0xffffff),
                warning: c(0xb7791f),
                danger: c(0xc2410c),
            },
            ShellThemeMode::Dark => ShellPalette {
                background: c(0x121314),
                surface: c(0x1c201d),
                page: c(0x121314),
                border: c(0x323a32),
                text: c(0xe6eadf),
                muted: c(0x9aa594),
                subtle: c(0x283029),
                hover: c(0x313a31),
                active: c(0x3fa0fe),
                active_hover: c(0x238cf0),
                active_text: c(0xffffff),
                warning: c(0xfbbf24),
                danger: c(0xfb7185),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellThemePreference {
    System,
    Light,
    Dark,
}

impl From<GuiThemePreference> for ShellThemePreference {
    fn from(value: GuiThemePreference) -> Self {
        match value {
            GuiThemePreference::System => Self::System,
            GuiThemePreference::Light => Self::Light,
            GuiThemePreference::Dark => Self::Dark,
        }
    }
}

impl From<ShellThemePreference> for GuiThemePreference {
    fn from(value: ShellThemePreference) -> Self {
        match value {
            ShellThemePreference::System => Self::System,
            ShellThemePreference::Light => Self::Light,
            ShellThemePreference::Dark => Self::Dark,
        }
    }
}

impl ShellThemePreference {
    fn resolved_mode(self, system_mode: ShellThemeMode) -> ShellThemeMode {
        match self {
            Self::System => system_mode,
            Self::Light => ShellThemeMode::Light,
            Self::Dark => ShellThemeMode::Dark,
        }
    }
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

#[derive(Clone, Copy)]
pub(crate) struct ShellPalette {
    pub(crate) background: Hsla,
    pub(crate) surface: Hsla,
    pub(crate) page: Hsla,
    pub(crate) border: Hsla,
    pub(crate) text: Hsla,
    pub(crate) muted: Hsla,
    pub(crate) subtle: Hsla,
    pub(crate) hover: Hsla,
    pub(crate) active: Hsla,
    pub(crate) active_hover: Hsla,
    pub(crate) active_text: Hsla,
    pub(crate) warning: Hsla,
    pub(crate) danger: Hsla,
}

pub struct Shell {
    active_route: AppRoute,
    snapshot: AppSnapshot,
    monitor: monitor::MonitorPageState,
    log_monitoring_active: bool,
    traffic_monitoring_active: bool,
    connections_monitoring_active: bool,
    status_tun_enabled: bool,
    status_runtime_mode: String,
    page_states_suspended_for_tray: bool,
    tray_cleanup_generation: u64,
    monitor_log_scroll_handle: VirtualListScrollHandle,
    log_runtime: Option<LogPageRuntime>,
    connection_detail_editor_contents: String,
    subscription_yaml_editor_contents: String,
    rules_proxy: rules::RulesProxyPageState,
    rules_proxy_scroll_handle: VirtualListScrollHandle,
    rules_proxy_runtime: Option<RulesProxyPageRuntime>,
    override_script: override_script::OverrideScriptPageState,
    override_preview_editor_contents: String,
    override_script_runtime: Option<OverrideScriptPageRuntime>,
    groups: proxy_groups::GroupPageState,
    group_runtime: Option<GroupPageRuntime>,
    connections: connections::ConnectionsPageState,
    connections_runtime: Option<ConnectionsPageRuntime>,
    subscriptions: subscriptions::SubscriptionPageState,
    subscription_runtime: Option<SubscriptionPageRuntime>,
    subscription_diagnostic_notices: BTreeSet<String>,
    settings: settings::SettingsPageState,
    command_router: Option<AppCommandRouter>,
    settings_inputs: settings::SettingsPageInputs,
    _settings_subscriptions: Vec<Subscription>,
    _shutdown_subscription: Option<Subscription>,
    config_editor: config_editor::ConfigEditorPageState,
    config_runtime: Option<ConfigEditorPageRuntime>,
    config_save_notices: BTreeSet<config_editor::ConfigEditorGroup>,
    theme_preference: ShellThemePreference,
    system_theme_mode: ShellThemeMode,
    _appearance_subscription: Subscription,
    _tray_handle: TrayHandle,
    pending_commands: BTreeMap<CommandId, AppCommand>,
    pending_after_config_save: BTreeMap<CommandId, AppCommand>,
    core_service_confirmation: Option<CoreServiceConfirmation>,
    app_quit_requested: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoreServiceConfirmation {
    Install,
    Uninstall,
}

struct LogPageRuntime {
    monitor_search_input: Entity<InputState>,
    _monitor_search_subscription: Subscription,
}

struct RulesProxyPageRuntime {
    search_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

struct OverrideScriptPageRuntime {
    editor: Entity<InputState>,
    preview_editor: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

struct GroupPageRuntime {
    search_input: Entity<InputState>,
    group_scroll_handle: ScrollHandle,
    member_scroll_handle: ScrollHandle,
    proxies_input: Entity<InputState>,
    providers_input: Entity<InputState>,
    filter_input: Entity<InputState>,
    exclude_filter_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

struct ConnectionsPageRuntime {
    inputs: connections::ConnectionsPageInputs,
    detail_editor: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

struct SubscriptionPageRuntime {
    inputs: subscriptions::SubscriptionPageInputs,
    _subscriptions: Vec<Subscription>,
}

struct ConfigEditorPageRuntime {
    inputs: config_editor::ConfigEditorInputs,
    _subscriptions: Vec<Subscription>,
}

mod actions;
mod config_loaders;
mod events;
mod input_bindings;
mod input_helpers;
mod lifecycle;
mod navigation;
mod render;
mod runtimes;
mod status_bar;
mod theme_runtime;

use config_loaders::*;
use events::*;
use input_bindings::*;
use input_helpers::*;
pub use lifecycle::launch;
use runtimes::*;
use status_bar::*;
use theme_runtime::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::actions::collect_subscription_diagnostic_notices;
    use crate::shell::lifecycle::{
        should_dispatch_subscription_refresh, should_hide_window_on_startup,
        should_run_traffic_monitoring, should_start_core_on_startup,
    };
    use crate::shell::render::app_ui_font_family;
    use air_mihomo::streams::StreamEvent;

    #[test]
    fn macos_ui_font_uses_concrete_core_text_family() {
        if cfg!(target_os = "macos") {
            assert_eq!(app_ui_font_family(), "Helvetica");
        } else {
            assert_eq!(app_ui_font_family(), ".SystemUIFont");
        }
    }

    #[test]
    fn app_menu_exposes_quit_item_and_command_q_binding() {
        let menus = lifecycle::app_menus();
        assert_eq!(menus.len(), 1);
        assert_eq!(menus[0].name.as_ref(), "Air");
        assert!(
            menus[0].items.iter().any(|item| {
                matches!(
                    item,
                    MenuItem::Action { name, action, .. }
                        if name.as_ref() == "退出 Air" && action.as_any().is::<Quit>()
                )
            }),
            "app menu should include the Air quit action"
        );

        let binding = lifecycle::app_quit_key_binding();
        assert!(binding.action().as_any().is::<Quit>());
        assert_eq!(binding.keystrokes().len(), 1);
        let keystroke = binding.keystrokes()[0].inner();
        assert_eq!(keystroke.key, "q");
        assert!(keystroke.modifiers.platform);
    }

    #[test]
    fn explicit_quit_overrides_tray_close_behavior() {
        let settings = AppSettings {
            close_window_behavior: CloseWindowBehavior::Tray,
            ..AppSettings::default()
        };

        assert!(lifecycle::should_close_window_for_intent(&settings, true));
        assert!(!lifecycle::should_close_window_for_intent(&settings, false));
    }

    #[test]
    fn status_tun_toggle_restarts_or_starts_after_config_save() {
        assert!(matches!(
            lifecycle::command_after_tun_config_save(&RuntimeStatus::Running),
            Some(AppCommand::RestartCore)
        ));
        assert!(matches!(
            lifecycle::command_after_tun_config_save(&RuntimeStatus::Idle),
            Some(AppCommand::StartCore)
        ));
        assert!(matches!(
            lifecycle::command_after_tun_config_save(&RuntimeStatus::Failed {
                message: "failed".into()
            }),
            Some(AppCommand::StartCore)
        ));
        assert!(lifecycle::command_after_tun_config_save(&RuntimeStatus::Starting).is_none());
        assert!(lifecycle::command_after_tun_config_save(&RuntimeStatus::Stopping).is_none());
    }

    #[test]
    fn proxy_groups_empty_state_has_no_runtime_data() {
        let state = proxy_groups::GroupPageState::empty();

        assert!(state.view_model().items.is_empty());
    }

    #[test]
    fn status_runtime_mode_survives_config_editor_reset() {
        let mut status_mode = status_runtime_mode_value("global");
        let mut editor = config_editor::ConfigEditorPageState::empty();

        editor.apply_persisted_runtime_mode("rule");

        assert_eq!(status_mode, "global");
        status_mode = status_runtime_mode_value("direct");
        assert_eq!(status_mode, "direct");
    }

    #[test]
    fn status_runtime_mode_loads_from_saved_config_when_editor_is_empty() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let paths = air_storage::AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).expect("services should initialize");
        services
            .save_current_config("mode: global\n")
            .expect("runtime mode fixture should save");
        let router = AppCommandRouter::new(services);

        let editor_mode = status_runtime_mode_value(
            &config_editor::ConfigEditorPageState::empty()
                .view_model()
                .draft
                .global
                .mode,
        );
        let status_mode =
            load_saved_runtime_mode(Some(&router)).unwrap_or_else(|| editor_mode.clone());

        assert_eq!(editor_mode, "rule");
        assert_eq!(status_mode, "global");
    }

    #[test]
    fn app_event_updates_snapshot_runtime_without_window_context() {
        let mut snapshot = AppSnapshot::default();
        let mut monitor = monitor::MonitorPageState::default();
        let mut groups = proxy_groups::GroupPageState::fake_for_test();
        let mut rules_proxy = rules::RulesProxyPageState::empty();
        let mut connections = connections::ConnectionsPageState::default();
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();

        let effect = apply_app_event_to_state(
            &mut snapshot,
            &mut monitor,
            &mut groups,
            &mut rules_proxy,
            &mut connections,
            &mut subscriptions,
            AppEvent::RuntimeStatusChanged(RuntimeStatus::Running),
        );

        assert_eq!(effect, ShellAppEventEffect::Redraw);
        assert_eq!(snapshot.runtime, RuntimeStatus::Running);
    }

    #[test]
    fn app_event_redacts_visible_error_before_ui_state() {
        let mut snapshot = AppSnapshot::default();
        let mut monitor = monitor::MonitorPageState::default();
        let mut groups = proxy_groups::GroupPageState::fake_for_test();
        let mut rules_proxy = rules::RulesProxyPageState::empty();
        let mut connections = connections::ConnectionsPageState::default();
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();

        let effect = apply_app_event_to_state(
            &mut snapshot,
            &mut monitor,
            &mut groups,
            &mut rules_proxy,
            &mut connections,
            &mut subscriptions,
            AppEvent::UserVisibleError {
                message: "request failed token=abc secret=def".to_string(),
            },
        );

        let ShellAppEventEffect::UserVisibleError(message) = effect else {
            panic!("visible error should produce a notification effect");
        };
        assert!(message.contains("token=***"));
        assert!(message.contains("secret=***"));
        assert!(!message.contains("abc"));
        assert_eq!(snapshot.last_error.as_deref(), Some(message.as_str()));
        assert!(
            subscriptions.view_model().notice.is_none(),
            "非订阅命令错误不应进入订阅页顶部提示"
        );
    }

    #[test]
    fn subscription_import_error_releases_importing_state() {
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();
        subscriptions.update_import_url("https://example.test/sub.yaml?token=secret");
        let command = subscriptions
            .import_url()
            .expect("valid import URL should create an app command");
        let _ = subscriptions.take_notice();
        assert_eq!(
            subscriptions.view_model().import_status,
            subscriptions::SubscriptionImportStatus::Importing
        );

        let mut pending = BTreeMap::new();
        pending.insert(CommandId(7), command);

        let changed = apply_subscription_import_error(
            &mut subscriptions,
            &pending,
            "download failed token=secret",
        );

        assert!(changed);
        let model = subscriptions.view_model();
        assert_eq!(
            model.import_status,
            subscriptions::SubscriptionImportStatus::Failed
        );
        assert!(
            model
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("token=***"))
        );
    }

    #[test]
    fn non_subscription_error_does_not_touch_subscription_notice() {
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();
        subscriptions.update_import_url("https://example.test/sub.yaml");
        let _ = subscriptions.import_url();
        let _ = subscriptions.take_notice();

        let mut pending = BTreeMap::new();
        pending.insert(CommandId(9), AppCommand::RefreshRules);

        let changed = apply_subscription_import_error(
            &mut subscriptions,
            &pending,
            "rules refresh failed token=secret",
        );

        assert!(!changed);
        let model = subscriptions.view_model();
        assert_eq!(
            model.import_status,
            subscriptions::SubscriptionImportStatus::Importing
        );
        assert!(model.notice.is_none());
    }

    #[test]
    fn stream_event_is_dispatched_to_monitor_and_connections() {
        let mut snapshot = AppSnapshot::default();
        let mut monitor = monitor::MonitorPageState::default();
        let mut groups = proxy_groups::GroupPageState::fake_for_test();
        let mut rules_proxy = rules::RulesProxyPageState::empty();
        let mut connections = connections::ConnectionsPageState::default();
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();

        let effect = apply_app_event_to_state(
            &mut snapshot,
            &mut monitor,
            &mut groups,
            &mut rules_proxy,
            &mut connections,
            &mut subscriptions,
            AppEvent::MihomoStreamEvent(StreamEvent::Connections(serde_json::json!({
                "connections": [{
                    "id": "abc",
                    "metadata": {"host": "example.test", "process": "Code.exe", "network": "tcp"},
                    "chains": ["DIRECT"],
                    "upload": 10,
                    "download": 20,
                    "uploadSpeed": 1,
                    "downloadSpeed": 2,
                    "start": "2026-05-22T10:00:00+08:00"
                }]
            }))),
        );

        assert_eq!(effect, ShellAppEventEffect::Redraw);
        assert_eq!(connections.view_model().items.len(), 1);
        assert_eq!(monitor.view_model().log_count, 0);
    }

    #[test]
    fn traffic_stream_event_updates_status_bar_outside_logs_route() {
        let mut snapshot = AppSnapshot::default();
        let mut monitor = monitor::MonitorPageState::default();
        let mut groups = proxy_groups::GroupPageState::fake_for_test();
        let mut rules_proxy = rules::RulesProxyPageState::empty();
        let mut connections = connections::ConnectionsPageState::default();
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();

        let effect = apply_app_event_to_active_state(
            AppRoute::ProxyGroups,
            &mut snapshot,
            &mut monitor,
            &mut groups,
            &mut rules_proxy,
            &mut connections,
            &mut subscriptions,
            AppEvent::MihomoStreamEvent(StreamEvent::Traffic {
                upload: 1536,
                download: 4096,
            }),
        );

        assert_eq!(effect, ShellAppEventEffect::Redraw);
        assert_eq!(monitor.upload_text(), "1.5 KiB/s");
        assert_eq!(monitor.download_text(), "4.0 KiB/s");
    }

    #[test]
    fn traffic_stream_event_updates_status_bar_on_connections_route() {
        let mut snapshot = AppSnapshot::default();
        let mut monitor = monitor::MonitorPageState::default();
        let mut groups = proxy_groups::GroupPageState::fake_for_test();
        let mut rules_proxy = rules::RulesProxyPageState::empty();
        let mut connections = connections::ConnectionsPageState::default();
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();

        let effect = apply_app_event_to_active_state(
            AppRoute::Connections,
            &mut snapshot,
            &mut monitor,
            &mut groups,
            &mut rules_proxy,
            &mut connections,
            &mut subscriptions,
            AppEvent::MihomoStreamEvent(StreamEvent::Traffic {
                upload: 2048,
                download: 8192,
            }),
        );

        assert_eq!(effect, ShellAppEventEffect::Redraw);
        assert_eq!(monitor.upload_text(), "2.0 KiB/s");
        assert_eq!(monitor.download_text(), "8.0 KiB/s");
    }

    #[test]
    fn suspended_global_event_ignores_page_stream_state() {
        let mut snapshot = AppSnapshot::default();

        let effect = apply_app_event_to_global_state(
            &mut snapshot,
            AppEvent::MihomoStreamEvent(StreamEvent::Connections(serde_json::json!({
                "connections": [{
                    "id": "hidden-window",
                    "metadata": {"host": "example.test", "process": "Code.exe", "network": "tcp"},
                    "chains": ["DIRECT"],
                    "upload": 10,
                    "download": 20,
                    "uploadSpeed": 1,
                    "downloadSpeed": 2,
                    "start": "2026-05-22T10:00:00+08:00"
                }]
            }))),
        );

        assert_eq!(effect, ShellAppEventEffect::None);
        assert_eq!(snapshot, AppSnapshot::default());

        let effect = apply_app_event_to_global_state(
            &mut snapshot,
            AppEvent::RuntimeStatusChanged(RuntimeStatus::Running),
        );

        assert_eq!(effect, ShellAppEventEffect::Redraw);
        assert_eq!(snapshot.runtime, RuntimeStatus::Running);
    }

    #[test]
    fn connections_state_event_updates_connections_page() {
        let mut snapshot = AppSnapshot::default();
        let mut monitor = monitor::MonitorPageState::default();
        let mut groups = proxy_groups::GroupPageState::fake_for_test();
        let mut rules_proxy = rules::RulesProxyPageState::empty();
        let mut connections = connections::ConnectionsPageState::default();
        let mut subscriptions = subscriptions::SubscriptionPageState::empty();

        let effect = apply_app_event_to_state(
            &mut snapshot,
            &mut monitor,
            &mut groups,
            &mut rules_proxy,
            &mut connections,
            &mut subscriptions,
            AppEvent::ConnectionsStateChanged(air_mihomo::ConnectionsResponse {
                connections: vec![serde_json::json!({
                    "id": "http-refresh",
                    "metadata": {"host": "example.test", "process": "Code.exe", "network": "tcp"},
                    "chains": ["Proxy"],
                    "upload": 10,
                    "download": 20,
                    "uploadSpeed": 1,
                    "downloadSpeed": 2,
                    "start": "2026-05-22T10:00:00+08:00"
                })],
                upload_total: 10,
                download_total: 20,
                memory: 0,
                extra: BTreeMap::new(),
            }),
        );

        assert_eq!(effect, ShellAppEventEffect::Redraw);
        assert_eq!(connections.view_model().items.len(), 1);
        assert_eq!(connections.view_model().active_count, 1);
    }

    #[test]
    fn subscription_refresh_poll_skips_overlapping_updates() {
        let pending = BTreeMap::new();
        assert!(should_dispatch_subscription_refresh(&pending));

        let mut pending = BTreeMap::new();
        pending.insert(CommandId(1), AppCommand::RefreshDueSubscriptions);
        assert!(!should_dispatch_subscription_refresh(&pending));

        let mut pending = BTreeMap::new();
        pending.insert(
            CommandId(2),
            AppCommand::UpdateSubscription {
                subscription_id: "work".into(),
            },
        );
        assert!(!should_dispatch_subscription_refresh(&pending));
    }

    #[test]
    fn subscription_diagnostics_are_only_emitted_once_per_distinct_failure() {
        let state = subscriptions::SubscriptionPageState::fake_for_test();
        let mut seen = BTreeSet::new();

        let first = collect_subscription_diagnostic_notices(&state, &mut seen);
        let second = collect_subscription_diagnostic_notices(&state, &mut seen);

        assert_eq!(first.len(), 3);
        assert!(first.iter().any(|(_, message)| message.contains("Backup")));
        assert!(
            first
                .iter()
                .any(|(_, message)| message.contains("Reserved Base64"))
        );
        assert!(second.is_empty());

        let mut next_state = subscriptions::SubscriptionPageState::fake_for_test();
        next_state.bump_cache_checked_at_for_test("backup", 60_000);
        let third = collect_subscription_diagnostic_notices(&next_state, &mut seen);

        assert_eq!(third.len(), 2);
        assert!(third.iter().all(|(_, message)| message.contains("Backup")));
    }

    #[test]
    fn traffic_monitoring_runs_for_running_core_regardless_of_route() {
        assert!(should_run_traffic_monitoring(
            AppRoute::Logs,
            false,
            &RuntimeStatus::Running,
        ));
        assert!(should_run_traffic_monitoring(
            AppRoute::Connections,
            false,
            &RuntimeStatus::Running,
        ));
        assert!(!should_run_traffic_monitoring(
            AppRoute::Logs,
            true,
            &RuntimeStatus::Running,
        ));
        assert!(!should_run_traffic_monitoring(
            AppRoute::Logs,
            false,
            &RuntimeStatus::Idle,
        ));
    }

    #[test]
    fn elevated_relaunch_flag_forces_startup_core_start() {
        let settings = AppSettings {
            start_core_after_launch: false,
            ..AppSettings::default()
        };

        assert!(should_start_core_on_startup(&settings, true));
        assert!(!should_start_core_on_startup(&settings, false));
    }

    #[test]
    fn silent_start_hides_only_when_tray_is_available() {
        let settings = AppSettings {
            silent_start: true,
            autostart: false,
            ..AppSettings::default()
        };

        assert!(should_hide_window_on_startup(&settings, true));
        assert!(!should_hide_window_on_startup(&settings, false));

        let visible_settings = AppSettings {
            silent_start: false,
            autostart: true,
            ..AppSettings::default()
        };
        assert!(!should_hide_window_on_startup(&visible_settings, true));
    }
}
