use super::*;
use air_settings::{AppSettings, CloseWindowBehavior, GuiThemePreference};
use gpui_component::IconName;

#[test]
fn merged_settings_has_expected_pages() {
    let state = SettingsPageState::new(AppSettings::default());
    assert_eq!(
        state.view_model().pages,
        vec![
            UnifiedSettingsPage::Application,
            UnifiedSettingsPage::Core,
            UnifiedSettingsPage::Tun,
            UnifiedSettingsPage::Sniffer,
            UnifiedSettingsPage::Dns,
        ]
    );
}

#[test]
fn application_settings_only_keep_requested_fields() {
    let mut state = SettingsPageState::new(AppSettings::default());
    state.set_theme(GuiThemePreference::Dark);
    state.toggle_bool(SettingsBoolField::StartCoreAfterLaunch);
    state.set_bool(SettingsBoolField::SilentStartup, true);
    state.set_bool(SettingsBoolField::SystemProxy, true);
    state.set_bool(SettingsBoolField::HideToTray, true);
    state.set_text(
        SettingsTextField::ProxyDelayTestUrl,
        "https://example.test/generate_204",
    );

    assert_eq!(state.settings().theme, GuiThemePreference::Dark);
    assert!(state.settings().start_core_after_launch);
    assert!(state.settings().silent_start);
    assert!(state.settings().system_proxy_enabled);
    assert_eq!(
        state.settings().proxy_delay_test_url,
        "https://example.test/generate_204"
    );
    assert_eq!(
        state.settings().close_window_behavior,
        CloseWindowBehavior::Tray
    );
}

#[test]
fn application_startup_switches_are_independent_settings() {
    let mut state = SettingsPageState::new(AppSettings::default());

    state.set_bool(SettingsBoolField::Autostart, true);
    assert!(state.settings().autostart);
    assert!(!state.settings().silent_start);

    state.set_bool(SettingsBoolField::SilentStartup, true);
    assert!(state.settings().autostart);
    assert!(state.settings().silent_start);

    state.set_bool(SettingsBoolField::Autostart, false);
    assert!(!state.settings().autostart);
    assert!(state.settings().silent_start);
}

#[test]
fn settings_sidebar_uses_component_icons_separate_from_titles() {
    assert_eq!(UnifiedSettingsPage::Application.title(), "应用");
    assert!(matches!(
        UnifiedSettingsPage::Application.sidebar_icon(),
        IconName::Settings2
    ));
}
