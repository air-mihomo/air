use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use air_config::subscriptions::{
    SubscriptionCacheMetadata, SubscriptionIndex, SubscriptionPipelineError, SubscriptionSource,
    SubscriptionTimestamp, SubscriptionUpdateCacheStore, SubscriptionUpdateResult,
};
use air_error::{AppResult, ConfigError, StorageError};
use async_trait::async_trait;

use super::{AppPaths, FileStore, StoredFormat};

const SUBSCRIPTION_INDEX_PATH: &str = "subscriptions/index.json";

#[derive(Clone, Debug)]
pub struct SubscriptionStore {
    paths: AppPaths,
    config_files: FileStore,
    cache_files: FileStore,
}

impl SubscriptionStore {
    pub fn new(paths: AppPaths) -> Self {
        let config_files = FileStore::new(paths.config_dir.clone(), paths.backups_dir.clone());
        let cache_files = FileStore::new(
            paths.subscription_cache_dir.clone(),
            paths.backups_dir.join("subscriptions"),
        );
        Self {
            paths,
            config_files,
            cache_files,
        }
    }

    pub fn load_sources(&self) -> AppResult<Vec<SubscriptionSource>> {
        let sources = self.load_index()?.sources;
        tracing::info!(count = sources.len(), "loaded subscription sources");
        Ok(sources)
    }

    pub fn load_cache_metadata(&self) -> AppResult<Vec<SubscriptionCacheMetadata>> {
        let caches = self.load_index()?.caches.into_values().collect::<Vec<_>>();
        tracing::info!(count = caches.len(), "loaded subscription cache metadata");
        Ok(caches)
    }

    pub fn cache_metadata(
        &self,
        subscription_id: &str,
    ) -> AppResult<Option<SubscriptionCacheMetadata>> {
        let metadata = self.load_index()?.caches.get(subscription_id).cloned();
        tracing::info!(
            subscription_id,
            found = metadata.is_some(),
            "loaded subscription cache metadata entry"
        );
        Ok(metadata)
    }

    pub fn save_source(&self, source: SubscriptionSource) -> AppResult<()> {
        tracing::info!(
            subscription_id = %source.id,
            enabled = source.enabled,
            "saving subscription source"
        );
        source
            .validate()
            .map_err(|error| ConfigError::Subscription(error.to_string()))?;
        let mut index = self.load_index()?;
        match index.find_source_mut(&source.id) {
            Some(existing) => *existing = source,
            None => index.sources.push(source),
        }
        self.save_index(&index)
    }

    pub fn reorder_sources(&self, ordered_ids: &[String]) -> AppResult<()> {
        tracing::info!(count = ordered_ids.len(), "reordering subscription sources");
        let mut index = self.load_index()?;
        if ordered_ids.len() != index.sources.len() {
            return Err(
                ConfigError::Subscription("订阅排序列表和当前订阅数量不一致".into()).into(),
            );
        }

        let mut sources_by_id = index
            .sources
            .into_iter()
            .map(|source| (source.id.clone(), source))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut reordered = Vec::with_capacity(ordered_ids.len());
        for id in ordered_ids {
            let Some(source) = sources_by_id.remove(id) else {
                return Err(ConfigError::Subscription(format!("订阅源不存在: {id}")).into());
            };
            reordered.push(source);
        }
        if let Some(id) = sources_by_id.keys().next() {
            return Err(ConfigError::Subscription(format!("订阅排序缺少订阅源: {id}")).into());
        }

        // 只调整索引里的 sources 顺序，缓存正文和元数据按 id 关联，不能跟随拖拽重写 YAML。
        index.sources = reordered;
        self.save_index(&index)
    }

    pub fn select_source(&self, subscription_id: &str) -> AppResult<SubscriptionSource> {
        tracing::info!(subscription_id, "selecting subscription source");
        let mut index = self.load_index()?;
        if index.find_source(subscription_id).is_none() {
            return Err(
                ConfigError::Subscription(format!("订阅源不存在: {subscription_id}")).into(),
            );
        }

        // 运行配置合并的边界是 enabled 订阅缓存：订阅源索引只保存来源与启用状态，
        // 缓存文件保存解析前 YAML，真正合并仍在 config::merge 中完成。
        for source in &mut index.sources {
            source.enabled = source.id == subscription_id;
        }
        let selected = index
            .find_source(subscription_id)
            .cloned()
            .expect("source existence checked before enabling");
        self.save_index(&index)?;
        Ok(selected)
    }

    pub fn remove_source(&self, subscription_id: &str) -> AppResult<()> {
        tracing::info!(subscription_id, "removing subscription source");
        let mut index = self.load_index()?;
        let original_len = index.sources.len();
        index.sources.retain(|source| source.id != subscription_id);
        if index.sources.len() == original_len {
            return Err(
                ConfigError::Subscription(format!("订阅源不存在: {subscription_id}")).into(),
            );
        }

        if let Some(cache) = index.caches.remove(subscription_id) {
            self.backup_cached_content(&cache)?;
            if let Some(path) = cache.content_path {
                let absolute = self.resolve_cache_path(&path)?;
                if absolute.exists() {
                    fs::remove_file(absolute).map_err(StorageError::Io)?;
                }
            }
        }
        self.save_index(&index)
    }

    pub fn import_local_subscription_file(
        &self,
        name: impl Into<String>,
        source_path: &Path,
    ) -> AppResult<SubscriptionSource> {
        tracing::info!(path = %source_path.display(), "importing local subscription file");
        let bytes = fs::read(source_path).map_err(StorageError::Io)?;
        let mut index = self.load_index()?;
        let name = normalize_subscription_name(name.into())?;
        let id = self.generate_unique_id(&name, &index);
        let source =
            SubscriptionSource::local_file(id.clone(), name, Some(source_path.to_path_buf()));
        let cache_path = relative_cache_path(&id);
        self.cache_files.write_bytes(&cache_path, &bytes)?;

        let mut cache = SubscriptionCacheMetadata::new(id.clone());
        cache.content_path = Some(cache_path);
        cache.apply_update(SubscriptionUpdateResult::imported(
            now_timestamp()?,
            bytes.len() as u64,
        ));

        index.sources.push(source.clone());
        index.caches.insert(id, cache);
        self.save_index(&index)?;
        tracing::info!(
            subscription_id = %source.id,
            bytes = bytes.len(),
            "imported local subscription file into cache"
        );
        Ok(source)
    }

    pub fn import_remote_subscription_cache(
        &self,
        source: SubscriptionSource,
        result: SubscriptionUpdateResult,
        content: &[u8],
    ) -> AppResult<SubscriptionSource> {
        tracing::info!(
            subscription_id = %source.id,
            bytes = content.len(),
            "importing remote subscription cache"
        );
        source
            .validate()
            .map_err(|error| ConfigError::Subscription(error.to_string()))?;
        let mut index = self.load_index()?;
        if index.find_source(&source.id).is_some() {
            return Err(ConfigError::Subscription(format!("订阅源已存在: {}", source.id)).into());
        }

        // URL 导入只有在下载和解析都完成后才写入索引。缓存正文先写入独立文件，
        // 索引只保存元数据和相对路径，后续运行配置合并仍通过读取缓存内容来获得订阅配置。
        let cache_path = relative_cache_path(&source.id);
        self.cache_files.write_bytes(&cache_path, content)?;

        let mut cache = SubscriptionCacheMetadata::new(source.id.clone());
        cache.content_path = Some(cache_path);
        cache.apply_update(result);

        index.caches.insert(source.id.clone(), cache);
        index.sources.push(source.clone());
        self.save_index(&index)?;
        Ok(source)
    }

    pub fn record_update_result(
        &self,
        subscription_id: &str,
        result: SubscriptionUpdateResult,
        content: Option<&[u8]>,
    ) -> AppResult<SubscriptionCacheMetadata> {
        tracing::info!(
            subscription_id,
            has_content = content.is_some(),
            "recording subscription update result"
        );
        let mut index = self.load_index()?;
        if index.find_source(subscription_id).is_none() {
            return Err(
                ConfigError::Subscription(format!("订阅源不存在: {subscription_id}")).into(),
            );
        }

        let mut cache = index
            .caches
            .remove(subscription_id)
            .unwrap_or_else(|| SubscriptionCacheMetadata::new(subscription_id));
        if let Some(content) = content {
            let cache_path = cache
                .content_path
                .clone()
                .unwrap_or_else(|| relative_cache_path(subscription_id));
            self.cache_files.write_bytes(&cache_path, content)?;
            cache.content_path = Some(cache_path);
        }
        cache.apply_update(result);
        index
            .caches
            .insert(subscription_id.to_string(), cache.clone());
        self.save_index(&index)?;
        tracing::info!(
            subscription_id,
            has_cache_path = cache.content_path.is_some(),
            "recorded subscription update result"
        );
        Ok(cache)
    }

    pub fn read_cached_content(&self, subscription_id: &str) -> AppResult<Option<Vec<u8>>> {
        tracing::info!(subscription_id, "reading cached subscription content");
        let Some(cache) = self.cache_metadata(subscription_id)? else {
            return Ok(None);
        };
        let Some(path) = cache.content_path else {
            return Ok(None);
        };
        let absolute = self.resolve_cache_path(&path)?;
        match fs::read(absolute) {
            Ok(bytes) => {
                tracing::info!(
                    subscription_id,
                    bytes = bytes.len(),
                    "loaded cached subscription content"
                );
                Ok(Some(bytes))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StorageError::Io(error).into()),
        }
    }

    fn load_index(&self) -> AppResult<SubscriptionIndex> {
        let target = self.paths.config_dir.join(SUBSCRIPTION_INDEX_PATH);
        tracing::debug!(path = %target.display(), "loading subscription index");
        match fs::read(&target) {
            Ok(bytes) => {
                let index: SubscriptionIndex =
                    serde_json::from_slice(&bytes).map_err(StorageError::Json)?;
                index
                    .validate()
                    .map_err(|error| ConfigError::Subscription(error.to_string()))?;
                tracing::info!(
                    path = %target.display(),
                    source_count = index.sources.len(),
                    cache_count = index.caches.len(),
                    "loaded subscription index"
                );
                Ok(index)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tracing::info!(path = %target.display(), "subscription index missing; using default");
                Ok(SubscriptionIndex::default())
            }
            Err(error) => Err(StorageError::Io(error).into()),
        }
    }

    fn save_index(&self, index: &SubscriptionIndex) -> AppResult<()> {
        tracing::info!(
            source_count = index.sources.len(),
            cache_count = index.caches.len(),
            "saving subscription index"
        );
        index
            .validate()
            .map_err(|error| ConfigError::Subscription(error.to_string()))?;
        self.config_files.write(
            Path::new(SUBSCRIPTION_INDEX_PATH),
            index,
            StoredFormat::Json,
        )
    }

    fn resolve_cache_path(&self, path: &Path) -> AppResult<PathBuf> {
        tracing::debug!(path = %path.display(), "resolving subscription cache path");
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(StorageError::UnsafePath(path.to_path_buf()).into());
        }
        if path.is_absolute() && !path.starts_with(&self.paths.subscription_cache_dir) {
            return Err(StorageError::UnsafePath(path.to_path_buf()).into());
        }
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(self.paths.subscription_cache_dir.join(path))
        }
    }

    fn backup_cached_content(
        &self,
        cache: &SubscriptionCacheMetadata,
    ) -> AppResult<Option<PathBuf>> {
        tracing::info!(subscription_id = %cache.subscription_id, "backing up cached subscription content");
        let Some(path) = cache.content_path.as_deref() else {
            return Ok(None);
        };
        let source = self.resolve_cache_path(path)?;
        if !source.exists() {
            return Ok(None);
        }

        let backup_dir = self.paths.backups_dir.join("subscriptions");
        fs::create_dir_all(&backup_dir).map_err(StorageError::Io)?;
        let timestamp = now_timestamp()?;
        let mut backup = backup_dir.join(format!("{}-{timestamp}.yaml.bak", cache.subscription_id));
        let mut suffix = 1usize;
        while backup.exists() {
            backup = backup_dir.join(format!(
                "{}-{timestamp}-{suffix}.yaml.bak",
                cache.subscription_id
            ));
            suffix += 1;
        }
        fs::copy(&source, &backup).map_err(StorageError::Io)?;
        tracing::info!(
            subscription_id = %cache.subscription_id,
            backup = %backup.display(),
            "backed up cached subscription content"
        );
        Ok(Some(backup))
    }

    fn generate_unique_id(&self, name: &str, index: &SubscriptionIndex) -> String {
        let base = slugify_subscription_name(name);
        let timestamp = now_timestamp().unwrap_or(0);
        let mut candidate = format!("{base}-{timestamp}");
        let mut suffix = 1usize;
        while index.find_source(&candidate).is_some() {
            candidate = format!("{base}-{timestamp}-{suffix}");
            suffix += 1;
        }
        candidate
    }
}

#[async_trait]
impl SubscriptionUpdateCacheStore for SubscriptionStore {
    async fn load_metadata(
        &self,
        subscription_id: &str,
    ) -> Result<Option<SubscriptionCacheMetadata>, SubscriptionPipelineError> {
        self.cache_metadata(subscription_id)
            .map_err(|error| SubscriptionPipelineError::Cache(error.to_string()))
    }

    async fn read_cached_content(
        &self,
        subscription_id: &str,
    ) -> Result<Option<Vec<u8>>, SubscriptionPipelineError> {
        self.read_cached_content(subscription_id)
            .map_err(|error| SubscriptionPipelineError::Cache(error.to_string()))
    }

    async fn record_update(
        &self,
        subscription_id: &str,
        result: SubscriptionUpdateResult,
        content: Option<&[u8]>,
    ) -> Result<SubscriptionCacheMetadata, SubscriptionPipelineError> {
        self.record_update_result(subscription_id, result, content)
            .map_err(|error| SubscriptionPipelineError::Cache(error.to_string()))
    }
}

fn relative_cache_path(id: &str) -> PathBuf {
    PathBuf::from(format!("{id}.yaml"))
}

fn normalize_subscription_name(name: String) -> AppResult<String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(ConfigError::Subscription("订阅源名称不能为空".into()).into());
    }
    Ok(name)
}

fn slugify_subscription_name(name: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for byte in name.bytes() {
        let next = if byte.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(byte.to_ascii_lowercase() as char)
        } else if !previous_dash {
            previous_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = next {
            slug.push(ch);
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "subscription".into()
    } else {
        slug
    }
}

fn now_timestamp() -> AppResult<SubscriptionTimestamp> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ConfigError::Subscription(format!("系统时间早于 Unix epoch: {error}")))?;
    Ok(duration.as_millis() as SubscriptionTimestamp)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use air_config::subscriptions::{
        SubscriptionRequestHeaders, SubscriptionSourceKind, SubscriptionUpdateOutcome,
        SubscriptionUrl,
    };

    fn store_in_temp() -> (tempfile::TempDir, SubscriptionStore) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        paths.init().unwrap();
        (temp, SubscriptionStore::new(paths))
    }

    #[test]
    fn persists_remote_source_configuration() {
        let (_temp, store) = store_in_temp();
        let source = SubscriptionSource {
            id: "work".to_string(),
            name: "Work".to_string(),
            url: Some(SubscriptionUrl::new(
                "https://example.test/sub?token=secret-token",
            )),
            update_interval_secs: Some(3600),
            user_agent: Some("clash.meta/v1.20.1".to_string()),
            request_headers: SubscriptionRequestHeaders::new(BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer secret-token".to_string(),
            )])),
            proxy: Some("DIRECT".to_string()),
            enabled: true,
            source_kind: SubscriptionSourceKind::Remote,
        };

        store.save_source(source.clone()).unwrap();
        let loaded = store.load_sources().unwrap();

        assert_eq!(loaded, vec![source]);
        let debug = format!("{:?}", loaded[0]);
        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("https://example.test"));
    }

    #[test]
    fn imports_local_subscription_file_into_cache() {
        let (temp, store) = store_in_temp();
        let source_path = temp.path().join("import.yaml");
        fs::write(&source_path, b"proxies: []\n").unwrap();

        let source = store
            .import_local_subscription_file("Imported", &source_path)
            .unwrap();
        let sources = store.load_sources().unwrap();
        let content = store.read_cached_content(&source.id).unwrap().unwrap();
        let cache = store.cache_metadata(&source.id).unwrap().unwrap();

        assert_eq!(sources.len(), 1);
        assert!(matches!(
            sources[0].source_kind,
            SubscriptionSourceKind::LocalFile { .. }
        ));
        assert_eq!(content, b"proxies: []\n");
        assert_eq!(
            cache.last_update.unwrap().outcome,
            SubscriptionUpdateOutcome::Imported
        );
        assert!(cache.last_success_at.is_some());
    }

    #[test]
    fn records_update_result_headers_and_cached_bytes() {
        let (_temp, store) = store_in_temp();
        store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();
        let mut result = SubscriptionUpdateResult::success(1000, 12);
        result.etag = Some("etag-1".to_string());
        result.last_modified = Some("Thu, 21 May 2026 00:00:00 GMT".to_string());

        let cache = store
            .record_update_result("work", result, Some(b"proxies: []\n"))
            .unwrap();
        let content = store.read_cached_content("work").unwrap().unwrap();

        assert_eq!(cache.etag.as_deref(), Some("etag-1"));
        assert_eq!(
            cache.last_modified.as_deref(),
            Some("Thu, 21 May 2026 00:00:00 GMT")
        );
        assert_eq!(cache.last_success_at, Some(1000));
        assert_eq!(content, b"proxies: []\n");
    }

    #[test]
    fn selecting_source_persists_single_enabled_subscription() {
        let (_temp, store) = store_in_temp();
        store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();
        store
            .save_source(SubscriptionSource::remote(
                "backup",
                "Backup",
                "https://backup.example.test/sub",
            ))
            .unwrap();

        let selected = store.select_source("backup").unwrap();
        let sources = store.load_sources().unwrap();

        assert_eq!(selected.id, "backup");
        assert!(
            !sources
                .iter()
                .find(|source| source.id == "work")
                .unwrap()
                .enabled
        );
        assert!(
            sources
                .iter()
                .find(|source| source.id == "backup")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn reorder_sources_persists_card_order_without_touching_cache() {
        let (_temp, store) = store_in_temp();
        store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();
        store
            .save_source(SubscriptionSource::remote(
                "backup",
                "Backup",
                "https://backup.example.test/sub",
            ))
            .unwrap();
        store
            .record_update_result(
                "work",
                SubscriptionUpdateResult::success(1000, 12),
                Some(b"proxies: []\n"),
            )
            .unwrap();

        store
            .reorder_sources(&["backup".to_string(), "work".to_string()])
            .unwrap();

        let sources = store.load_sources().unwrap();
        assert_eq!(sources[0].id, "backup");
        assert_eq!(sources[1].id, "work");
        assert_eq!(
            store.read_cached_content("work").unwrap().unwrap(),
            b"proxies: []\n"
        );
    }

    #[test]
    fn records_failed_update_with_redacted_message() {
        let (_temp, store) = store_in_temp();
        store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();

        let cache = store
            .record_update_result(
                "work",
                SubscriptionUpdateResult::failed(
                    2000,
                    "GET https://example.test/sub?token=secret authorization=BearerSecret",
                ),
                None,
            )
            .unwrap();
        let debug = format!("{cache:?}");

        assert_eq!(cache.last_failure_at, Some(2000));
        assert!(!debug.contains("BearerSecret"));
        assert!(!debug.contains("token=secret"));
        assert!(debug.contains("token=***"));
    }

    #[test]
    fn remove_source_deletes_cache_and_keeps_backup() {
        let (temp, store) = store_in_temp();
        store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();
        store
            .record_update_result(
                "work",
                SubscriptionUpdateResult::success(1000, 12),
                Some(b"proxies: []\n"),
            )
            .unwrap();

        store.remove_source("work").unwrap();

        assert!(store.load_sources().unwrap().is_empty());
        assert!(store.read_cached_content("work").unwrap().is_none());
        let backup_dir = temp.path().join("data/backups/subscriptions");
        assert_eq!(fs::read_dir(backup_dir).unwrap().count(), 1);
    }
}
