use serde::{Deserialize, Deserializer, Serialize};

use air_error::{AppResult, ConfigError};

pub const DEFAULT_PROXY_DELAY_TEST_URL: &str = "http://cp.cloudflare.com/generate_204";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct AppSettings {
    pub theme: GuiThemePreference,
    pub language: AppLanguage,
    pub restore_window: bool,
    pub start_core_after_launch: bool,
    pub autostart: bool,
    pub silent_start: bool,
    pub override_script_enabled: bool,
    pub system_proxy_enabled: bool,
    pub proxy_delay_test_url: String,
    pub close_window_behavior: CloseWindowBehavior,
    pub window: WindowSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: GuiThemePreference::System,
            language: AppLanguage::ZhCn,
            restore_window: true,
            start_core_after_launch: false,
            autostart: false,
            silent_start: false,
            override_script_enabled: false,
            system_proxy_enabled: false,
            proxy_delay_test_url: DEFAULT_PROXY_DELAY_TEST_URL.to_string(),
            close_window_behavior: CloseWindowBehavior::Exit,
            window: WindowSettings::default(),
        }
    }
}

impl AppSettings {
    pub fn validate(&self) -> AppResult<()> {
        if self.window.width < 640 || self.window.height < 420 {
            tracing::error!(
                target: "air::validation",
                scope = "app-settings",
                field = "window",
                width = self.window.width,
                height = self.window.height,
                "app settings validation failed: window is smaller than 640x420"
            );
            return Err(ConfigError::Validation("窗口尺寸不能小于 640x420".into()).into());
        }
        if self.language != AppLanguage::ZhCn {
            tracing::error!(
                target: "air::validation",
                scope = "app-settings",
                field = "language",
                language = ?self.language,
                "app settings validation failed: unsupported language"
            );
            return Err(ConfigError::Validation("当前版本仅支持中文界面".into()).into());
        }
        // 应用自身设置没有诊断列表，成功时记录关键约束值，便于排查配置文件兼容问题。
        tracing::info!(
            target: "air::validation",
            scope = "app-settings",
            width = self.window.width,
            height = self.window.height,
            language = ?self.language,
            proxy_delay_test_url = %self.normalized_proxy_delay_test_url(),
            "app settings validation completed"
        );
        Ok(())
    }

    pub fn normalized_proxy_delay_test_url(&self) -> &str {
        let trimmed = self.proxy_delay_test_url.trim();
        if trimmed.is_empty() {
            DEFAULT_PROXY_DELAY_TEST_URL
        } else {
            trimmed
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuiThemePreference {
    System,
    Light,
    Dark,
}

impl GuiThemePreference {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppLanguage {
    ZhCn,
}

impl AppLanguage {
    pub fn label(self) -> &'static str {
        match self {
            Self::ZhCn => "中文",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloseWindowBehavior {
    Exit,
    Tray,
}

impl CloseWindowBehavior {
    pub fn label(self) -> &'static str {
        match self {
            Self::Exit => "退出",
            Self::Tray => "托盘",
        }
    }
}

impl<'de> Deserialize<'de> for AppSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(default, rename_all = "kebab-case")]
        struct AppSettingsCompat {
            theme: GuiThemePreference,
            language: AppLanguage,
            restore_window: bool,
            start_core_after_launch: bool,
            autostart: LegacyAutostartValue,
            silent_start: Option<bool>,
            override_script_enabled: bool,
            system_proxy_enabled: bool,
            proxy_delay_test_url: String,
            close_window_behavior: CloseWindowBehavior,
            window: WindowSettings,
        }

        impl Default for AppSettingsCompat {
            fn default() -> Self {
                let defaults = AppSettings::default();
                Self {
                    theme: defaults.theme,
                    language: defaults.language,
                    restore_window: defaults.restore_window,
                    start_core_after_launch: defaults.start_core_after_launch,
                    autostart: LegacyAutostartValue::Bool(defaults.autostart),
                    silent_start: None,
                    override_script_enabled: defaults.override_script_enabled,
                    system_proxy_enabled: defaults.system_proxy_enabled,
                    proxy_delay_test_url: defaults.proxy_delay_test_url,
                    close_window_behavior: defaults.close_window_behavior,
                    window: defaults.window,
                }
            }
        }

        let compat = AppSettingsCompat::deserialize(deserializer)?;
        let legacy_silent = compat.autostart.is_legacy_silent();
        Ok(Self {
            theme: compat.theme,
            language: compat.language,
            restore_window: compat.restore_window,
            start_core_after_launch: compat.start_core_after_launch,
            autostart: compat.autostart.enabled(),
            // 旧版 `autostart = "silent"` 同时表达自启和静默启动；新版拆成两个字段后只在迁移时继承该语义。
            silent_start: compat.silent_start.unwrap_or(legacy_silent),
            override_script_enabled: compat.override_script_enabled,
            system_proxy_enabled: compat.system_proxy_enabled,
            proxy_delay_test_url: compat.proxy_delay_test_url,
            close_window_behavior: compat.close_window_behavior,
            window: compat.window,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyAutostartValue {
    Bool(bool),
    Off,
    On,
    Silent,
}

impl LegacyAutostartValue {
    fn enabled(self) -> bool {
        matches!(self, Self::Bool(true) | Self::On | Self::Silent)
    }

    fn is_legacy_silent(self) -> bool {
        matches!(self, Self::Silent)
    }
}

impl<'de> Deserialize<'de> for LegacyAutostartValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }

        match Raw::deserialize(deserializer)? {
            Raw::Bool(value) => Ok(Self::Bool(value)),
            Raw::Text(value) => match value.as_str() {
                "off" => Ok(Self::Off),
                "on" => Ok(Self::On),
                "silent" => Ok(Self::Silent),
                other => Err(serde::de::Error::custom(format!(
                    "未知开机自启策略: {other}"
                ))),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct WindowSettings {
    pub width: u32,
    pub height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    pub maximized: bool,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: 1080,
            height: 720,
            x: None,
            y: None,
            maximized: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_match_supported_runtime_options() {
        let settings = AppSettings::default();

        assert_eq!(settings.theme, GuiThemePreference::System);
        assert_eq!(settings.language, AppLanguage::ZhCn);
        assert!(settings.restore_window);
        assert!(!settings.autostart);
        assert!(!settings.silent_start);
        assert!(!settings.override_script_enabled);
        assert!(!settings.system_proxy_enabled);
        assert_eq!(settings.proxy_delay_test_url, DEFAULT_PROXY_DELAY_TEST_URL);
        assert_eq!(settings.close_window_behavior, CloseWindowBehavior::Exit);
    }

    #[test]
    fn legacy_silent_autostart_is_migrated_to_two_switches() {
        let settings: AppSettings = toml::from_str(
            r#"
theme = "system"
language = "zh-cn"
restore-window = true
start-core-after-launch = false
autostart = "silent"
close-window-behavior = "exit"

[window]
width = 1080
height = 720
maximized = false
"#,
        )
        .unwrap();

        assert!(settings.autostart);
        assert!(settings.silent_start);
        assert_eq!(settings.proxy_delay_test_url, DEFAULT_PROXY_DELAY_TEST_URL);
    }

    #[test]
    fn empty_proxy_delay_url_falls_back_to_default_at_use_site() {
        let settings = AppSettings {
            proxy_delay_test_url: "   ".into(),
            ..AppSettings::default()
        };

        assert_eq!(
            settings.normalized_proxy_delay_test_url(),
            DEFAULT_PROXY_DELAY_TEST_URL
        );
    }

    #[test]
    fn rejects_unsupported_language_and_tiny_window() {
        let mut settings = AppSettings::default();
        settings.window.width = 320;

        assert!(settings.validate().is_err());
    }
}
