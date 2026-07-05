use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use air_error::{AppResult, StorageError};
use air_platform::system_proxy::SystemProxySnapshot;
use air_storage::{AppPaths, FileStore, StoredFormat};

pub const SYSTEM_PROXY_SNAPSHOT_PATH: &str = "system-proxy-snapshot.json";

#[derive(Clone, Debug)]
pub struct SystemProxySnapshotStore {
    paths: AppPaths,
    files: FileStore,
}

impl SystemProxySnapshotStore {
    pub fn new(paths: AppPaths) -> Self {
        let files = FileStore::new(paths.data_dir.clone(), paths.backups_dir.clone());
        Self { paths, files }
    }

    pub fn load(&self) -> AppResult<Option<SystemProxySnapshot>> {
        let path = self.snapshot_path();
        match self
            .files
            .read(Path::new(SYSTEM_PROXY_SNAPSHOT_PATH), StoredFormat::Json)
        {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(air_error::AppError::Storage(StorageError::Io(error)))
                if error.kind() == ErrorKind::NotFound =>
            {
                tracing::debug!(path = %path.display(), "system proxy snapshot missing");
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, snapshot: &SystemProxySnapshot) -> AppResult<()> {
        self.files.write(
            Path::new(SYSTEM_PROXY_SNAPSHOT_PATH),
            snapshot,
            StoredFormat::Json,
        )
    }

    pub fn delete(&self) -> AppResult<()> {
        let path = self.snapshot_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StorageError::Io(error).into()),
        }
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.paths.data_dir.join(SYSTEM_PROXY_SNAPSHOT_PATH)
    }
}

#[cfg(test)]
mod tests {
    use air_platform::system_proxy::{MacosProxyState, SystemProxyServiceSnapshot};

    use super::*;

    fn store_in_temp() -> (tempfile::TempDir, SystemProxySnapshotStore) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        paths.init().unwrap();
        (temp, SystemProxySnapshotStore::new(paths))
    }

    fn sample_snapshot() -> SystemProxySnapshot {
        SystemProxySnapshot {
            managed_port: 9870,
            services: vec![SystemProxyServiceSnapshot {
                service: "Wi-Fi".into(),
                web: MacosProxyState {
                    enabled: true,
                    server: Some("proxy.example.test".into()),
                    port: Some(8080),
                    authenticated: false,
                },
                secure_web: MacosProxyState::default(),
                socks: MacosProxyState::default(),
            }],
        }
    }

    #[test]
    fn load_missing_snapshot_returns_none() {
        let (_temp, store) = store_in_temp();

        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn saves_loads_and_deletes_snapshot_in_data_dir() {
        let (temp, store) = store_in_temp();
        let snapshot = sample_snapshot();

        store.save(&snapshot).unwrap();
        assert!(temp.path().join("data/system-proxy-snapshot.json").exists());
        assert_eq!(store.load().unwrap(), Some(snapshot));

        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
