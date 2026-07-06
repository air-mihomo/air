//! 订阅源领域模型。
//!
//! Air 的订阅源是 GUI 自己维护的上层元数据：它记录远程 URL、请求头、缓存状态和手动导入来源。
//! mihomo 的 `proxy-providers` 是运行时配置的一部分，负责按 provider 名称把节点注入代理组。
//! 后续合并任务可以把订阅解析结果转换为 `proxy-providers` 或直接合并到 `proxies`，但本模型本身
//! 不下载、不解析，也不直接改写 mihomo 主配置。

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::model::{MihomoConfigDocument, ProxyNode};

use air_telemetry::redaction::redact_log_value;

pub type SubscriptionTimestamp = u64;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubscriptionUrl(String);

impl SubscriptionUrl {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn redacted_label(&self) -> &'static str {
        "<redacted-url>"
    }
}

impl fmt::Debug for SubscriptionUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.redacted_label())
    }
}

#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubscriptionRequestHeaders(BTreeMap<String, String>);

impl SubscriptionRequestHeaders {
    pub fn new(headers: BTreeMap<String, String>) -> Self {
        Self(headers)
    }

    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) -> Option<String> {
        self.0.insert(name.into(), value.into())
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }
}

impl Deref for SubscriptionRequestHeaders {
    type Target = BTreeMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for SubscriptionRequestHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted = self
            .0
            .iter()
            .map(|(name, value)| {
                let value = if is_sensitive_header(name) {
                    "<redacted>"
                } else {
                    value.as_str()
                };
                (name, value)
            })
            .collect::<BTreeMap<_, _>>();
        formatter.debug_map().entries(redacted).finish()
    }
}

/// 订阅源配置。远程订阅必须有 `url`，手动导入文件会把 `url` 留空并记录 `source_kind`。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SubscriptionSource {
    pub id: String,
    pub name: String,
    pub url: Option<SubscriptionUrl>,
    pub update_interval_secs: Option<u64>,
    pub user_agent: Option<String>,
    pub request_headers: SubscriptionRequestHeaders,
    pub proxy: Option<String>,
    pub enabled: bool,
    pub source_kind: SubscriptionSourceKind,
}

impl SubscriptionSource {
    pub fn remote(id: impl Into<String>, name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            url: Some(SubscriptionUrl::new(url)),
            source_kind: SubscriptionSourceKind::Remote,
            ..Self::default()
        }
    }

    pub fn local_file(
        id: impl Into<String>,
        name: impl Into<String>,
        imported_from: Option<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            url: None,
            source_kind: SubscriptionSourceKind::LocalFile { imported_from },
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), SubscriptionValidationError> {
        if self.id.trim().is_empty() {
            log_subscription_source_validation(self, Some(&SubscriptionValidationError::EmptyId));
            return Err(SubscriptionValidationError::EmptyId);
        }
        if self.name.trim().is_empty() {
            log_subscription_source_validation(self, Some(&SubscriptionValidationError::EmptyName));
            return Err(SubscriptionValidationError::EmptyName);
        }
        if matches!(self.source_kind, SubscriptionSourceKind::Remote) && self.url.is_none() {
            log_subscription_source_validation(
                self,
                Some(&SubscriptionValidationError::MissingRemoteUrl),
            );
            return Err(SubscriptionValidationError::MissingRemoteUrl);
        }
        if self
            .request_headers
            .keys()
            .any(|name| name.trim().is_empty())
        {
            log_subscription_source_validation(
                self,
                Some(&SubscriptionValidationError::EmptyHeaderName),
            );
            return Err(SubscriptionValidationError::EmptyHeaderName);
        }
        log_subscription_source_validation(self, None);
        Ok(())
    }

    pub fn redacted_summary(&self) -> SubscriptionSourceSummary {
        SubscriptionSourceSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            has_url: self.url.is_some(),
            update_interval_secs: self.update_interval_secs,
            proxy: self.proxy.clone(),
            enabled: self.enabled,
            source_kind: self.source_kind.clone(),
        }
    }
}

impl Default for SubscriptionSource {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            url: None,
            update_interval_secs: Some(86_400),
            user_agent: None,
            request_headers: SubscriptionRequestHeaders::default(),
            proxy: None,
            enabled: true,
            source_kind: SubscriptionSourceKind::Remote,
        }
    }
}

impl fmt::Debug for SubscriptionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionSource")
            .field("id", &self.id)
            .field("name", &self.name)
            .field(
                "url",
                &self.url.as_ref().map(SubscriptionUrl::redacted_label),
            )
            .field("update_interval_secs", &self.update_interval_secs)
            .field("user_agent", &self.user_agent)
            .field("request_headers", &self.request_headers)
            .field("proxy", &self.proxy)
            .field("enabled", &self.enabled)
            .field("source_kind", &self.source_kind)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionSourceKind {
    Remote,
    LocalFile { imported_from: Option<PathBuf> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SubscriptionSourceSummary {
    pub id: String,
    pub name: String,
    pub has_url: bool,
    pub update_interval_secs: Option<u64>,
    pub proxy: Option<String>,
    pub enabled: bool,
    pub source_kind: SubscriptionSourceKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SubscriptionIndex {
    pub sources: Vec<SubscriptionSource>,
    pub caches: BTreeMap<String, SubscriptionCacheMetadata>,
}

impl SubscriptionIndex {
    pub fn validate(&self) -> Result<(), SubscriptionValidationError> {
        let mut ids = BTreeMap::<&str, usize>::new();
        for source in &self.sources {
            if let Err(error) = source.validate() {
                log_subscription_index_validation(self, Some(&error));
                return Err(error);
            }
            if ids.insert(source.id.as_str(), 1).is_some() {
                let error = SubscriptionValidationError::DuplicateId(source.id.clone());
                log_subscription_index_validation(self, Some(&error));
                return Err(error);
            }
        }
        log_subscription_index_validation(self, None);
        Ok(())
    }

    pub fn find_source(&self, id: &str) -> Option<&SubscriptionSource> {
        self.sources.iter().find(|source| source.id == id)
    }

    pub fn find_source_mut(&mut self, id: &str) -> Option<&mut SubscriptionSource> {
        self.sources.iter_mut().find(|source| source.id == id)
    }
}

/// 本地缓存元数据只描述最近一次更新/导入结果；缓存内容本体由存储层放到独立文件。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SubscriptionCacheMetadata {
    pub subscription_id: String,
    pub content_path: Option<PathBuf>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_success_at: Option<SubscriptionTimestamp>,
    pub last_failure_at: Option<SubscriptionTimestamp>,
    pub last_update: Option<SubscriptionUpdateResult>,
}

impl SubscriptionCacheMetadata {
    pub fn new(subscription_id: impl Into<String>) -> Self {
        Self {
            subscription_id: subscription_id.into(),
            ..Self::default()
        }
    }

    pub fn apply_update(&mut self, result: SubscriptionUpdateResult) {
        if let Some(etag) = result.etag.clone() {
            self.etag = Some(etag);
        }
        if let Some(last_modified) = result.last_modified.clone() {
            self.last_modified = Some(last_modified);
        }
        match result.outcome {
            SubscriptionUpdateOutcome::Success
            | SubscriptionUpdateOutcome::NotModified
            | SubscriptionUpdateOutcome::Imported => self.last_success_at = Some(result.checked_at),
            SubscriptionUpdateOutcome::Failed => self.last_failure_at = Some(result.checked_at),
            SubscriptionUpdateOutcome::Canceled => {}
        }
        self.last_update = Some(result.redacted());
    }
}

impl Default for SubscriptionCacheMetadata {
    fn default() -> Self {
        Self {
            subscription_id: String::new(),
            content_path: None,
            etag: None,
            last_modified: None,
            last_success_at: None,
            last_failure_at: None,
            last_update: None,
        }
    }
}

impl fmt::Debug for SubscriptionCacheMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionCacheMetadata")
            .field("subscription_id", &self.subscription_id)
            .field("content_path", &self.content_path)
            .field("etag", &self.etag)
            .field("last_modified", &self.last_modified)
            .field("last_success_at", &self.last_success_at)
            .field("last_failure_at", &self.last_failure_at)
            .field("last_update", &self.last_update)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SubscriptionUpdateResult {
    pub checked_at: SubscriptionTimestamp,
    pub outcome: SubscriptionUpdateOutcome,
    pub status_code: Option<u16>,
    pub bytes: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub user_info: Option<SubscriptionUserInfo>,
    pub message: Option<String>,
}

impl SubscriptionUpdateResult {
    pub fn success(checked_at: SubscriptionTimestamp, bytes: u64) -> Self {
        Self {
            checked_at,
            outcome: SubscriptionUpdateOutcome::Success,
            bytes: Some(bytes),
            ..Self::default()
        }
    }

    pub fn imported(checked_at: SubscriptionTimestamp, bytes: u64) -> Self {
        Self {
            checked_at,
            outcome: SubscriptionUpdateOutcome::Imported,
            bytes: Some(bytes),
            ..Self::default()
        }
    }

    pub fn failed(checked_at: SubscriptionTimestamp, message: impl AsRef<str>) -> Self {
        Self {
            checked_at,
            outcome: SubscriptionUpdateOutcome::Failed,
            message: Some(redact_log_value(message.as_ref())),
            ..Self::default()
        }
    }

    pub fn canceled(checked_at: SubscriptionTimestamp, message: impl AsRef<str>) -> Self {
        Self {
            checked_at,
            outcome: SubscriptionUpdateOutcome::Canceled,
            message: Some(redact_log_value(message.as_ref())),
            ..Self::default()
        }
    }

    pub fn redacted(mut self) -> Self {
        if let Some(message) = self.message.as_deref() {
            self.message = Some(redact_log_value(message));
        }
        self
    }
}

impl Default for SubscriptionUpdateResult {
    fn default() -> Self {
        Self {
            checked_at: 0,
            outcome: SubscriptionUpdateOutcome::Success,
            status_code: None,
            bytes: None,
            etag: None,
            last_modified: None,
            user_info: None,
            message: None,
        }
    }
}

impl fmt::Debug for SubscriptionUpdateResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionUpdateResult")
            .field("checked_at", &self.checked_at)
            .field("outcome", &self.outcome)
            .field("status_code", &self.status_code)
            .field("bytes", &self.bytes)
            .field("etag", &self.etag)
            .field("last_modified", &self.last_modified)
            .field("user_info", &self.user_info)
            .field("message", &self.message.as_deref().map(redact_log_value))
            .finish()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SubscriptionUserInfo {
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub total: Option<u64>,
    // subscription-userinfo 的 expire 使用 Unix 秒；UI 展示时再换算为上海时区。
    pub expire: Option<SubscriptionTimestamp>,
}

impl SubscriptionUserInfo {
    pub fn used_bytes(&self) -> Option<u64> {
        match (self.upload, self.download) {
            (Some(upload), Some(download)) => Some(upload.saturating_add(download)),
            (Some(upload), None) => Some(upload),
            (None, Some(download)) => Some(download),
            (None, None) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionUpdateOutcome {
    #[default]
    Success,
    NotModified,
    Failed,
    Imported,
    Canceled,
}

#[async_trait]
pub trait SubscriptionUpdateCacheStore: Send + Sync {
    async fn load_metadata(
        &self,
        subscription_id: &str,
    ) -> Result<Option<SubscriptionCacheMetadata>, SubscriptionPipelineError>;

    async fn read_cached_content(
        &self,
        subscription_id: &str,
    ) -> Result<Option<Vec<u8>>, SubscriptionPipelineError>;

    async fn record_update(
        &self,
        subscription_id: &str,
        result: SubscriptionUpdateResult,
        content: Option<&[u8]>,
    ) -> Result<SubscriptionCacheMetadata, SubscriptionPipelineError>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedSubscription {
    pub format: SubscriptionContentFormat,
    pub document: MihomoConfigDocument,
    pub proxies: Vec<ProxyNode>,
    pub diagnostics: Vec<SubscriptionDiagnostic>,
}

impl ParsedSubscription {
    /// Returns the content that should be persisted in subscription cache.
    pub fn cache_content(
        &self,
        original_content: &str,
    ) -> Result<Vec<u8>, SubscriptionPipelineError> {
        match self.format {
            SubscriptionContentFormat::MihomoYaml => Ok(original_content.as_bytes().to_vec()),
            SubscriptionContentFormat::Base64Nodes => serde_yaml::to_string(&self.document)
                .map(|yaml| yaml.into_bytes())
                .map_err(|error| {
                    SubscriptionPipelineError::InvalidResponse(vec![SubscriptionDiagnostic::error(
                        "base64-cache-yaml",
                        format!("base64 订阅转换后的 YAML 序列化失败: {error}"),
                    )])
                }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionContentFormat {
    MihomoYaml,
    Base64Nodes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SubscriptionDiagnostic {
    pub severity: SubscriptionDiagnosticSeverity,
    pub code: String,
    pub message: String,
}

impl SubscriptionDiagnostic {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: SubscriptionDiagnosticSeverity::Error,
            code: code.into(),
            message: redact_log_value(&message.into()),
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: SubscriptionDiagnosticSeverity::Warning,
            code: code.into(),
            message: redact_log_value(&message.into()),
        }
    }
}

impl Default for SubscriptionDiagnostic {
    fn default() -> Self {
        Self {
            severity: SubscriptionDiagnosticSeverity::Info,
            code: String::new(),
            message: String::new(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SubscriptionPipelineError {
    #[error("订阅源已禁用: {0}")]
    Disabled(String),
    #[error("订阅请求无效: {0}")]
    InvalidRequest(String),
    #[error("订阅下载失败: {0}")]
    Network(String),
    #[error("订阅响应状态异常: {status}")]
    HttpStatus {
        status: u16,
        diagnostics: Vec<SubscriptionDiagnostic>,
    },
    #[error("订阅响应无效")]
    InvalidResponse(Vec<SubscriptionDiagnostic>),
    #[error("订阅解析失败")]
    Parse {
        diagnostics: Vec<SubscriptionDiagnostic>,
    },
    #[error("订阅缓存失败: {0}")]
    Cache(String),
}

impl SubscriptionPipelineError {
    pub fn diagnostics(&self) -> Vec<SubscriptionDiagnostic> {
        match self {
            Self::Disabled(source_id) => vec![SubscriptionDiagnostic::error(
                "subscription-disabled",
                format!("订阅源已禁用: {source_id}"),
            )],
            Self::InvalidRequest(message) => {
                vec![SubscriptionDiagnostic::error(
                    "invalid-request",
                    message.clone(),
                )]
            }
            Self::Network(message) => vec![SubscriptionDiagnostic::error(
                "network",
                format!("订阅下载失败: {message}"),
            )],
            Self::HttpStatus { diagnostics, .. }
            | Self::InvalidResponse(diagnostics)
            | Self::Parse { diagnostics } => diagnostics.clone(),
            Self::Cache(message) => vec![SubscriptionDiagnostic::error("cache", message.clone())],
        }
    }

    pub fn safe_message(&self) -> String {
        redact_log_value(&format!("{self:?} {self}"))
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SubscriptionValidationError {
    #[error("订阅源 id 不能为空")]
    EmptyId,
    #[error("订阅源名称不能为空")]
    EmptyName,
    #[error("远程订阅源缺少 URL")]
    MissingRemoteUrl,
    #[error("订阅源请求头名称不能为空")]
    EmptyHeaderName,
    #[error("订阅源 id 重复: {0}")]
    DuplicateId(String),
}

fn log_subscription_source_validation(
    source: &SubscriptionSource,
    error: Option<&SubscriptionValidationError>,
) {
    match error {
        Some(error) => tracing::error!(
            target: "air::validation",
            scope = "subscription-source",
            subscription_id = %source.id,
            name = %source.name,
            source_kind = ?source.source_kind,
            has_url = source.url.is_some(),
            enabled = source.enabled,
            error = %redact_log_value(&error.to_string()),
            "subscription source validation failed"
        ),
        None => tracing::info!(
            target: "air::validation",
            scope = "subscription-source",
            subscription_id = %source.id,
            name = %source.name,
            source_kind = ?source.source_kind,
            has_url = source.url.is_some(),
            enabled = source.enabled,
            "subscription source validation completed"
        ),
    }
}

fn log_subscription_index_validation(
    index: &SubscriptionIndex,
    error: Option<&SubscriptionValidationError>,
) {
    match error {
        Some(error) => tracing::error!(
            target: "air::validation",
            scope = "subscription-index",
            sources = index.sources.len(),
            caches = index.caches.len(),
            error = %redact_log_value(&error.to_string()),
            "subscription index validation failed"
        ),
        None => tracing::info!(
            target: "air::validation",
            scope = "subscription-index",
            sources = index.sources.len(),
            caches = index.caches.len(),
            "subscription index validation completed"
        ),
    }
}

fn is_sensitive_header(name: &str) -> bool {
    let normalized = name
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_')
        .collect::<String>()
        .to_ascii_lowercase();
    normalized == "authorization"
        || normalized == "proxyauthorization"
        || normalized == "cookie"
        || normalized == "setcookie"
        || normalized.contains("token")
        || normalized.contains("apikey")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_url_and_auth_headers() {
        let mut headers = SubscriptionRequestHeaders::default();
        headers.insert("Authorization", "Bearer secret-token");
        headers.insert("Accept", "text/yaml");
        let mut source = SubscriptionSource::remote(
            "work",
            "Work",
            "https://example.test/sub?token=secret-token",
        );
        source.request_headers = headers;

        let debug = format!("{source:?}");

        assert!(debug.contains("<redacted-url>"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("text/yaml"));
        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("https://example.test"));
    }

    #[test]
    fn update_result_redacts_failure_message() {
        let result = SubscriptionUpdateResult::failed(
            42,
            "failed https://example.test/sub?token=secret authorization=BearerSecret",
        );
        let debug = format!("{result:?}");

        assert!(!debug.contains("BearerSecret"));
        assert!(!debug.contains("token=secret"));
        assert!(debug.contains("token=***"));
    }
}
