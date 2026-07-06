use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use air_error::{AppResult, StorageError};
use air_settings::AppSettings;

use super::{AppPaths, FileStore};

const APP_CONFIG_PATH: &str = "app.config.toml";

#[derive(Clone, Debug)]
pub struct SettingsStore {
    paths: AppPaths,
    files: FileStore,
}

impl SettingsStore {
    pub fn new(paths: AppPaths) -> Self {
        let files = FileStore::new(paths.config_dir.clone(), paths.backups_dir.clone());
        Self { paths, files }
    }

    pub fn load(&self) -> AppResult<AppSettings> {
        let target = self.paths.config_dir.join(APP_CONFIG_PATH);
        tracing::info!(path = %target.display(), "loading app settings");
        match fs::read_to_string(&target) {
            Ok(source) => {
                let settings: AppSettings = toml::from_str(&source).map_err(|error| {
                    StorageError::Toml(format!("读取 app.config.toml 失败: {error}"))
                })?;
                settings.validate()?;
                tracing::info!(
                    theme = ?settings.theme,
                    language = ?settings.language,
                    autostart = settings.autostart,
                    silent_start = settings.silent_start,
                    override_script_enabled = settings.override_script_enabled,
                    proxy_delay_test_url = %settings.normalized_proxy_delay_test_url(),
                    "loaded app settings"
                );
                Ok(settings)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tracing::info!(path = %target.display(), "app settings file missing; using defaults");
                Ok(AppSettings::default())
            }
            Err(error) => Err(StorageError::Io(error).into()),
        }
    }

    pub fn ensure_exists(&self) -> AppResult<AppSettings> {
        let target = self.paths.config_dir.join(APP_CONFIG_PATH);
        let settings = self.load()?;
        if !target.exists() {
            tracing::info!(path = %target.display(), "persisting default app settings file");
            self.save(&settings)?;
        }
        Ok(settings)
    }

    pub fn save(&self, settings: &AppSettings) -> AppResult<()> {
        settings.validate()?;
        tracing::info!(
            path = %self.paths.config_dir.join(APP_CONFIG_PATH).display(),
            theme = ?settings.theme,
            language = ?settings.language,
            autostart = settings.autostart,
            silent_start = settings.silent_start,
            override_script_enabled = settings.override_script_enabled,
            proxy_delay_test_url = %settings.normalized_proxy_delay_test_url(),
            "saving app settings"
        );
        let bytes = toml::to_string_pretty(settings)
            .map_err(|error| StorageError::Toml(format!("写入 app.config.toml 失败: {error}")))?
            .into_bytes();
        // 应用设置只有一个权威文件；仍沿用 FileStore 的原子替换和备份策略，避免异常退出留下半截 TOML。
        self.files.write_bytes(Path::new(APP_CONFIG_PATH), &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use air_settings::{
        AppLanguage, CloseWindowBehavior, DEFAULT_PROXY_DELAY_TEST_URL, GuiThemePreference,
    };

    fn store_in_temp() -> (tempfile::TempDir, SettingsStore) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        paths.init().unwrap();
        (temp, SettingsStore::new(paths))
    }

    #[test]
    fn missing_app_config_uses_defaults() {
        let (_temp, store) = store_in_temp();

        let settings = store.load().unwrap();

        assert_eq!(settings.theme, GuiThemePreference::System);
        assert_eq!(settings.language, AppLanguage::ZhCn);
        assert!(settings.restore_window);
        assert!(!settings.autostart);
        assert!(!settings.silent_start);
        assert!(!settings.override_script_enabled);
        assert_eq!(settings.proxy_delay_test_url, DEFAULT_PROXY_DELAY_TEST_URL);
        assert_eq!(settings.close_window_behavior, CloseWindowBehavior::Exit);
    }

    #[test]
    fn ensure_exists_creates_default_app_config() {
        let (temp, store) = store_in_temp();

        let settings = store.ensure_exists().unwrap();

        assert_eq!(settings, AppSettings::default());
        assert!(temp.path().join("config/app.config.toml").exists());
    }

    #[test]
    fn saves_app_config_as_toml_without_nulls() {
        let (temp, store) = store_in_temp();
        let mut settings = AppSettings::default();
        settings.theme = GuiThemePreference::Dark;
        settings.autostart = true;
        settings.silent_start = true;
        settings.override_script_enabled = true;
        settings.system_proxy_enabled = true;
        settings.proxy_delay_test_url = "https://probe.example.test/generate_204".into();
        settings.close_window_behavior = CloseWindowBehavior::Tray;

        store.save(&settings).unwrap();
        store.save(&settings).unwrap();

        let source = fs::read_to_string(temp.path().join("config/app.config.toml")).unwrap();
        assert!(source.contains("theme = \"dark\""));
        assert!(source.contains("autostart = true"));
        assert!(source.contains("silent-start = true"));
        assert!(source.contains("override-script-enabled = true"));
        assert!(source.contains("system-proxy-enabled = true"));
        assert!(
            source.contains("proxy-delay-test-url = \"https://probe.example.test/generate_204\"")
        );
        assert!(source.contains("close-window-behavior = \"tray\""));
        assert!(!source.contains("null"));
        assert!(
            temp.path()
                .join("data/backups/app.config.toml.bak")
                .exists()
        );
        assert!(store.load().unwrap().autostart);
        assert!(store.load().unwrap().silent_start);
    }

    #[test]
    fn rejects_tiny_window() {
        let (_temp, store) = store_in_temp();
        let mut settings = AppSettings::default();
        settings.window.width = 320;

        assert!(store.save(&settings).is_err());
    }
}
