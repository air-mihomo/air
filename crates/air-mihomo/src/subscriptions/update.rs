//! 订阅更新、条件请求和解析流水线。
//!
//! 本模块只负责下载、解析和写入订阅缓存，不直接修改当前 profile。失败路径会记录失败元数据，
//! 但不会传入新的缓存正文，因此上一份成功内容会继续保留，供后续合并任务或 UI 回退使用。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{
    CONTENT_DISPOSITION, ETAG, HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE,
    IF_NONE_MATCH, LAST_MODIFIED, USER_AGENT,
};
use reqwest::{Client, Proxy};
use serde_yaml::Value;

use air_config::model::MihomoConfigDocument;
use air_config::subscriptions::{
    ParsedSubscription, SubscriptionCacheMetadata, SubscriptionContentFormat,
    SubscriptionDiagnostic, SubscriptionDiagnosticSeverity, SubscriptionPipelineError,
    SubscriptionSource, SubscriptionTimestamp, SubscriptionUpdateCacheStore,
    SubscriptionUpdateOutcome, SubscriptionUpdateResult, SubscriptionUserInfo,
};
use air_telemetry::redaction::redact_log_value;

const SUBSCRIPTION_USERINFO: &str = "subscription-userinfo";

#[derive(Clone, Debug)]
pub struct SubscriptionUpdatePipeline<S> {
    client: reqwest::Client,
    store: S,
    parser: SubscriptionParser,
    core_version: Option<String>,
}

impl<S> SubscriptionUpdatePipeline<S>
where
    S: SubscriptionUpdateCacheStore,
{
    pub fn new(store: S) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("fixed reqwest client options should be valid");
        Self::with_client(store, client)
    }

    pub fn with_client(store: S, client: reqwest::Client) -> Self {
        Self {
            client,
            store,
            parser: SubscriptionParser::default(),
            core_version: None,
        }
    }

    pub fn with_core_version(mut self, version: Option<String>) -> Self {
        self.core_version = version;
        self
    }

    pub async fn update(
        &self,
        source: &SubscriptionSource,
    ) -> Result<SubscriptionPipelineReport, SubscriptionPipelineError> {
        self.update_with_options(source, SubscriptionUpdateOptions::default())
            .await
    }

    pub async fn update_inactive_cache(
        &self,
        source: &SubscriptionSource,
    ) -> Result<SubscriptionPipelineReport, SubscriptionPipelineError> {
        self.update_with_options(
            source,
            SubscriptionUpdateOptions {
                allow_disabled_source: true,
            },
        )
        .await
    }

    async fn update_with_options(
        &self,
        source: &SubscriptionSource,
        options: SubscriptionUpdateOptions,
    ) -> Result<SubscriptionPipelineReport, SubscriptionPipelineError> {
        if !options.allow_disabled_source && !source.enabled {
            return Err(SubscriptionPipelineError::Disabled(source.id.clone()));
        }
        source
            .validate()
            .map_err(|error| SubscriptionPipelineError::InvalidRequest(error.to_string()))?;

        let previous = self.store.load_metadata(&source.id).await?;
        let response = match self.fetch(source, previous.as_ref()).await {
            Ok(response) => response,
            Err(error) => {
                self.record_failure(&source.id, previous.as_ref(), &error)
                    .await?;
                return Err(error);
            }
        };

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return self
                .reuse_cached_content(source, previous.as_ref(), response.status().as_u16())
                .await;
        }

        if !response.status().is_success() {
            let error = SubscriptionPipelineError::HttpStatus {
                status: response.status().as_u16(),
                diagnostics: vec![SubscriptionDiagnostic::error(
                    "http-status",
                    format!("订阅下载返回 HTTP {}", response.status().as_u16()),
                )],
            };
            self.record_failure(&source.id, previous.as_ref(), &error)
                .await?;
            return Err(error);
        }

        let status_code = response.status().as_u16();
        let etag = header_to_string(response.headers(), ETAG);
        let last_modified = header_to_string(response.headers(), LAST_MODIFIED);
        let display_name = header_to_string(response.headers(), CONTENT_DISPOSITION)
            .and_then(|value| subscription_name_from_content_disposition(&value));
        let user_info = response
            .headers()
            .get(SUBSCRIPTION_USERINFO)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_subscription_user_info);
        let content = match response.text().await {
            Ok(content) => content,
            Err(error) => {
                let error = SubscriptionPipelineError::Network(redact_log_value(
                    &error.without_url().to_string(),
                ));
                self.record_failure(&source.id, previous.as_ref(), &error)
                    .await?;
                return Err(error);
            }
        };
        let parsed = match self.parser.parse(&content) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.record_failure(&source.id, previous.as_ref(), &error)
                    .await?;
                return Err(error);
            }
        };

        let mut result = SubscriptionUpdateResult::success(now_timestamp(), content.len() as u64);
        result.status_code = Some(status_code);
        result.etag = etag;
        result.last_modified = last_modified;
        result.user_info = user_info;
        let metadata = self
            .store
            .record_update(&source.id, result, Some(content.as_bytes()))
            .await?;

        Ok(SubscriptionPipelineReport {
            subscription_id: source.id.clone(),
            status: SubscriptionPipelineStatus::Updated,
            display_name,
            metadata,
            parsed,
        })
    }

    async fn fetch(
        &self,
        source: &SubscriptionSource,
        previous: Option<&SubscriptionCacheMetadata>,
    ) -> Result<reqwest::Response, SubscriptionPipelineError> {
        let url = source
            .url
            .as_ref()
            .ok_or_else(|| SubscriptionPipelineError::InvalidRequest("远程订阅缺少 URL".into()))?;
        let url = reqwest::Url::parse(url.as_str()).map_err(|error| {
            SubscriptionPipelineError::InvalidRequest(format!(
                "订阅 URL 无效 {}: {error}",
                redact_log_value(url.as_str())
            ))
        })?;
        let client = build_subscription_http_client(&self.client, source)?;
        let mut request = client.get(url);

        for (name, value) in source.request_headers.iter() {
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                SubscriptionPipelineError::InvalidRequest(format!("订阅请求头名称无效: {name}"))
            })?;
            let header_value = HeaderValue::from_str(value).map_err(|_| {
                // 只输出头名称，不输出值，避免 Authorization/Cookie 等凭据进入日志。
                SubscriptionPipelineError::InvalidRequest(format!("订阅请求头 `{name}` 的值无效"))
            })?;
            request = request.header(header_name, header_value);
        }
        request = request.header(
            USER_AGENT,
            source_subscription_user_agent(source, self.core_version.as_deref()),
        );

        if let Some(previous) = previous {
            if let Some(etag) = previous.etag.as_deref() {
                request = request.header(IF_NONE_MATCH, etag);
            }
            if let Some(last_modified) = previous.last_modified.as_deref() {
                request = request.header(IF_MODIFIED_SINCE, last_modified);
            }
        }

        request.send().await.map_err(|error| {
            SubscriptionPipelineError::Network(redact_log_value(&error.without_url().to_string()))
        })
    }

    async fn reuse_cached_content(
        &self,
        source: &SubscriptionSource,
        previous: Option<&SubscriptionCacheMetadata>,
        status_code: u16,
    ) -> Result<SubscriptionPipelineReport, SubscriptionPipelineError> {
        let content = self
            .store
            .read_cached_content(&source.id)
            .await?
            .ok_or_else(|| {
                SubscriptionPipelineError::InvalidResponse(vec![SubscriptionDiagnostic::error(
                    "not-modified-cache-missing",
                    "远端返回 304 Not Modified，但本地没有可复用的成功缓存",
                )])
            })?;
        let content = std::str::from_utf8(&content).map_err(|error| {
            SubscriptionPipelineError::InvalidResponse(vec![SubscriptionDiagnostic::error(
                "cache-utf8",
                format!("订阅缓存不是有效 UTF-8: {error}"),
            )])
        })?;
        let parsed = self.parser.parse(content)?;

        let mut result = SubscriptionUpdateResult::success(now_timestamp(), content.len() as u64);
        result.outcome = SubscriptionUpdateOutcome::NotModified;
        result.status_code = Some(status_code);
        result.user_info = previous
            .and_then(|metadata| metadata.last_update.as_ref())
            .and_then(|last_update| last_update.user_info.clone());
        let metadata = self.store.record_update(&source.id, result, None).await?;

        Ok(SubscriptionPipelineReport {
            subscription_id: source.id.clone(),
            status: SubscriptionPipelineStatus::NotModified,
            display_name: None,
            metadata,
            parsed,
        })
    }

    async fn record_failure(
        &self,
        subscription_id: &str,
        previous: Option<&SubscriptionCacheMetadata>,
        error: &SubscriptionPipelineError,
    ) -> Result<(), SubscriptionPipelineError> {
        let mut result = SubscriptionUpdateResult::failed(now_timestamp(), error.safe_message());
        // 失败响应可能仍带有 subscription-userinfo，但正文或状态无效时不能用它覆盖旧的
        // 流量信息；这里只沿用已持久化的值，避免 UI 在更新失败后把套餐用量清空或写成脏数据。
        result.user_info = previous
            .and_then(|metadata| metadata.last_update.as_ref())
            .and_then(|last_update| last_update.user_info.clone());
        self.store
            .record_update(subscription_id, result, None)
            .await
            .map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SubscriptionUpdateOptions {
    /// 手动刷新禁用订阅时只更新本地缓存，不能改变它是否参与运行配置合并。
    allow_disabled_source: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionPipelineStatus {
    Updated,
    NotModified,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptionPipelineReport {
    pub subscription_id: String,
    pub status: SubscriptionPipelineStatus,
    pub display_name: Option<String>,
    pub metadata: SubscriptionCacheMetadata,
    pub parsed: ParsedSubscription,
}

fn log_subscription_diagnostics(
    scope: &str,
    proxy_count: usize,
    diagnostics: &[SubscriptionDiagnostic],
) {
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == SubscriptionDiagnosticSeverity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == SubscriptionDiagnosticSeverity::Warning)
        .count();
    let infos = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == SubscriptionDiagnosticSeverity::Info)
        .count();

    // Subscription validation never logs the source URL or content body.
    if errors > 0 {
        tracing::warn!(
            target: "air::validation",
            scope,
            proxy_count,
            errors,
            warnings,
            infos,
            total = diagnostics.len(),
            "subscription validation completed with blocking errors"
        );
    } else {
        tracing::info!(
            target: "air::validation",
            scope,
            proxy_count,
            errors,
            warnings,
            infos,
            total = diagnostics.len(),
            "subscription validation completed"
        );
    }

    for diagnostic in diagnostics {
        let message = redact_log_value(&diagnostic.message);
        match diagnostic.severity {
            SubscriptionDiagnosticSeverity::Error => tracing::error!(
                target: "air::validation",
                scope,
                severity = "error",
                code = %diagnostic.code,
                message = %message,
                "subscription validation diagnostic"
            ),
            SubscriptionDiagnosticSeverity::Warning => tracing::warn!(
                target: "air::validation",
                scope,
                severity = "warning",
                code = %diagnostic.code,
                message = %message,
                "subscription validation diagnostic"
            ),
            SubscriptionDiagnosticSeverity::Info => tracing::info!(
                target: "air::validation",
                scope,
                severity = "info",
                code = %diagnostic.code,
                message = %message,
                "subscription validation diagnostic"
            ),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SubscriptionParser {
    base64_parser: ReservedBase64NodeParser,
}

impl SubscriptionParser {
    pub fn parse(&self, content: &str) -> Result<ParsedSubscription, SubscriptionPipelineError> {
        let raw = match serde_yaml::from_str::<Value>(content) {
            Ok(raw) => raw,
            Err(_) if is_probably_base64_subscription(content) => {
                return self.base64_parser.parse(content);
            }
            Err(error) => {
                let diagnostics = vec![SubscriptionDiagnostic::error(
                    "yaml-syntax",
                    format!("订阅 YAML 语法无效: {error}"),
                )];
                log_subscription_diagnostics("subscription-parser", 0, &diagnostics);
                return Err(SubscriptionPipelineError::Parse { diagnostics });
            }
        };

        if !matches!(raw, Value::Mapping(_)) {
            if is_probably_base64_subscription(content) {
                return self.base64_parser.parse(content);
            }
            let diagnostics = vec![SubscriptionDiagnostic::error(
                "yaml-root",
                "订阅 YAML 顶层必须是对象",
            )];
            log_subscription_diagnostics("subscription-parser", 0, &diagnostics);
            return Err(SubscriptionPipelineError::Parse { diagnostics });
        }

        let document: MihomoConfigDocument = serde_yaml::from_value(raw).map_err(|error| {
            let diagnostics = vec![SubscriptionDiagnostic::error(
                "yaml-schema",
                format!("订阅 YAML 字段格式无效: {error}"),
            )];
            log_subscription_diagnostics("subscription-parser", 0, &diagnostics);
            SubscriptionPipelineError::Parse { diagnostics }
        })?;
        let proxies = document.proxies.clone();
        let mut diagnostics = Vec::new();
        if proxies.is_empty() {
            diagnostics.push(SubscriptionDiagnostic::warning(
                "empty-proxies",
                "订阅 YAML 未包含 proxies 节点列表",
            ));
        }
        log_subscription_diagnostics("subscription-parser", proxies.len(), &diagnostics);

        Ok(ParsedSubscription {
            format: SubscriptionContentFormat::MihomoYaml,
            document,
            proxies,
            diagnostics,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReservedBase64NodeParser;

impl ReservedBase64NodeParser {
    pub fn parse(&self, _content: &str) -> Result<ParsedSubscription, SubscriptionPipelineError> {
        let diagnostics = vec![SubscriptionDiagnostic::error(
            "base64-parser-reserved",
            "base64 节点订阅解析接口已预留，当前版本尚未实现转换",
        )];
        log_subscription_diagnostics("subscription-base64-parser", 0, &diagnostics);
        Err(SubscriptionPipelineError::Parse { diagnostics })
    }
}

fn now_timestamp() -> SubscriptionTimestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as SubscriptionTimestamp
}

fn header_to_string(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn build_subscription_http_client(
    base_client: &Client,
    source: &SubscriptionSource,
) -> Result<Client, SubscriptionPipelineError> {
    let Some(proxy) = source
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(base_client.clone());
    };

    if proxy.eq_ignore_ascii_case("DIRECT") {
        return Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .map_err(|error| {
                SubscriptionPipelineError::InvalidRequest(format!(
                    "订阅更新代理配置无效: {}",
                    redact_log_value(&error.to_string())
                ))
            });
    }

    if !looks_like_proxy_url(proxy) {
        return Err(SubscriptionPipelineError::InvalidRequest(
            "订阅更新代理仅支持 DIRECT 或 http(s)/socks5 代理 URL".to_string(),
        ));
    }

    let proxy = Proxy::all(proxy).map_err(|error| {
        SubscriptionPipelineError::InvalidRequest(format!(
            "订阅更新代理无效: {}",
            redact_log_value(&error.to_string())
        ))
    })?;

    Client::builder()
        .timeout(Duration::from_secs(30))
        .proxy(proxy)
        .build()
        .map_err(|error| {
            SubscriptionPipelineError::InvalidRequest(format!(
                "订阅更新代理配置无效: {}",
                redact_log_value(&error.to_string())
            ))
        })
}

fn looks_like_proxy_url(value: &str) -> bool {
    matches!(
        url::Url::parse(value).ok().map(|url| url.scheme().to_ascii_lowercase()),
        Some(scheme) if matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h")
    )
}

fn is_probably_base64_subscription(content: &str) -> bool {
    let compact = content.trim();
    compact.len() >= 16
        && compact.lines().count() <= 3
        && compact
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '\r' | '\n'))
}

fn subscription_download_user_agent(core_version: Option<&str>) -> String {
    let version = core_version
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(|version| version.trim_start_matches('v'))
        .filter(|version| !version.is_empty())
        .unwrap_or("0.0.0");
    format!("clash.meta/v{version}")
}

fn source_subscription_user_agent(
    source: &SubscriptionSource,
    core_version: Option<&str>,
) -> String {
    source
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| subscription_download_user_agent(core_version))
}

fn parse_subscription_user_info(value: &str) -> Option<SubscriptionUserInfo> {
    let mut info = SubscriptionUserInfo::default();
    for part in value.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "upload" => info.upload = Some(value),
            "download" => info.download = Some(value),
            "total" => info.total = Some(value),
            "expire" => info.expire = Some(value.saturating_mul(1000)),
            _ => {}
        }
    }
    (info.upload.is_some()
        || info.download.is_some()
        || info.total.is_some()
        || info.expire.is_some())
    .then_some(info)
}

fn subscription_name_from_content_disposition(value: &str) -> Option<String> {
    let mut fallback = None;
    for part in value.split(';').map(str::trim) {
        let Some((name, raw_value)) = part.split_once('=') else {
            continue;
        };
        if name.eq_ignore_ascii_case("filename*") {
            let value = decode_rfc5987_filename(raw_value.trim()).or_else(|| {
                clean_content_disposition_filename(raw_value.trim())
                    .and_then(|value| percent_decode(&value))
            });
            if let Some(value) = value.and_then(|value| normalize_content_disposition_name(&value))
            {
                return Some(value);
            }
        } else if name.eq_ignore_ascii_case("filename") {
            fallback = clean_content_disposition_filename(raw_value.trim());
        }
    }
    fallback
}

fn decode_rfc5987_filename(value: &str) -> Option<String> {
    let value = value.trim_matches('"');
    let (_, rest) = value.split_once("''")?;
    percent_decode(rest)
}

fn clean_content_disposition_filename(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"').trim();
    let value = value.rsplit(['/', '\\']).next().unwrap_or(value).trim();
    normalize_content_disposition_name(value)
}

fn normalize_content_disposition_name(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_end_matches(".yaml")
        .trim_end_matches(".yml")
        .trim()
        .to_string();
    (!value.is_empty()).then_some(value)
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let raw = value.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' && index + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[index + 1..index + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            bytes.push(byte);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;
    use air_mihomo::subscriptions::{SubscriptionRequestHeaders, SubscriptionUrl};
    use async_trait::async_trait;

    #[derive(Clone, Default)]
    struct MemoryUpdateStore {
        metadata: Arc<Mutex<BTreeMap<String, SubscriptionCacheMetadata>>>,
        content: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    }

    impl MemoryUpdateStore {
        fn insert_success(&self, id: &str, content: &[u8]) {
            self.content
                .lock()
                .expect("content lock should not be poisoned")
                .insert(id.to_string(), content.to_vec());
            let mut metadata = SubscriptionCacheMetadata::new(id);
            metadata.etag = Some("\"etag-a\"".to_string());
            metadata.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());
            let mut result = SubscriptionUpdateResult::success(1, content.len() as u64);
            result.etag = metadata.etag.clone();
            result.last_modified = metadata.last_modified.clone();
            metadata.apply_update(result);
            self.metadata
                .lock()
                .expect("metadata lock should not be poisoned")
                .insert(id.to_string(), metadata);
        }

        fn insert_success_with_user_info(
            &self,
            id: &str,
            content: &[u8],
            user_info: SubscriptionUserInfo,
        ) {
            self.insert_success(id, content);
            let mut metadata = self
                .metadata(id)
                .expect("inserted metadata should be available");
            let mut result = metadata
                .last_update
                .clone()
                .expect("inserted metadata should have last_update");
            result.user_info = Some(user_info);
            metadata.apply_update(result);
            self.metadata
                .lock()
                .expect("metadata lock should not be poisoned")
                .insert(id.to_string(), metadata);
        }

        fn cached_content(&self, id: &str) -> Option<Vec<u8>> {
            self.content
                .lock()
                .expect("content lock should not be poisoned")
                .get(id)
                .cloned()
        }

        fn metadata(&self, id: &str) -> Option<SubscriptionCacheMetadata> {
            self.metadata
                .lock()
                .expect("metadata lock should not be poisoned")
                .get(id)
                .cloned()
        }
    }

    #[async_trait]
    impl SubscriptionUpdateCacheStore for MemoryUpdateStore {
        async fn load_metadata(
            &self,
            subscription_id: &str,
        ) -> Result<Option<SubscriptionCacheMetadata>, SubscriptionPipelineError> {
            Ok(self.metadata(subscription_id))
        }

        async fn read_cached_content(
            &self,
            subscription_id: &str,
        ) -> Result<Option<Vec<u8>>, SubscriptionPipelineError> {
            Ok(self.cached_content(subscription_id))
        }

        async fn record_update(
            &self,
            subscription_id: &str,
            result: SubscriptionUpdateResult,
            content: Option<&[u8]>,
        ) -> Result<SubscriptionCacheMetadata, SubscriptionPipelineError> {
            if let Some(content) = content {
                self.content
                    .lock()
                    .expect("content lock should not be poisoned")
                    .insert(subscription_id.to_string(), content.to_vec());
            }
            let mut metadata = self
                .metadata(subscription_id)
                .unwrap_or_else(|| SubscriptionCacheMetadata::new(subscription_id));
            metadata.apply_update(result);
            self.metadata
                .lock()
                .expect("metadata lock should not be poisoned")
                .insert(subscription_id.to_string(), metadata.clone());
            Ok(metadata)
        }
    }

    struct FakeResponse {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
    }

    struct FakeHttpServer {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FakeHttpServer {
        fn spawn(responses: Vec<FakeResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);

            thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().expect("fake request should arrive");
                    let mut buffer = [0_u8; 8192];
                    let size = stream
                        .read(&mut buffer)
                        .expect("request should be readable");
                    captured
                        .lock()
                        .expect("request lock should not be poisoned")
                        .push(String::from_utf8_lossy(&buffer[..size]).to_string());

                    let mut header_text = format!(
                        "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                        response.status,
                        response.body.len()
                    );
                    for (name, value) in response.headers {
                        header_text.push_str(&format!("{name}: {value}\r\n"));
                    }
                    header_text.push_str("\r\n");
                    stream
                        .write_all(header_text.as_bytes())
                        .expect("headers should be writable");
                    stream
                        .write_all(response.body.as_bytes())
                        .expect("body should be writable");
                }
            });

            Self {
                url: format!("http://{addr}/sub.yaml?token=secret-token"),
                requests,
            }
        }

        fn first_request(&self) -> String {
            self.requests
                .lock()
                .expect("request lock should not be poisoned")
                .first()
                .cloned()
                .expect("request should be captured")
        }
    }

    fn yaml_subscription() -> &'static str {
        r#"
proxies:
  - name: alpha
    type: ss
    server: example.com
    port: 8388
    cipher: aes-128-gcm
    password: keep-secret
"#
    }

    fn source(url: String) -> SubscriptionSource {
        let mut source = SubscriptionSource::remote("sub-a", "Sub A", url);
        source.user_agent = Some("clash.meta/v9.9.9".to_string());
        source
    }

    #[tokio::test]
    async fn downloads_yaml_subscription_and_saves_cache() {
        let server = FakeHttpServer::spawn(vec![FakeResponse {
            status: 200,
            headers: vec![
                ("ETag", "\"etag-a\""),
                ("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
                ("Content-Disposition", "attachment;filename*=UTF-8''SSRDOG"),
                (
                    "Subscription-Userinfo",
                    "upload=546551043; download=758785499; total=322122547200; expire=1816327723",
                ),
            ],
            body: yaml_subscription(),
        }]);
        let store = MemoryUpdateStore::default();
        let pipeline = SubscriptionUpdatePipeline::with_client(
            store.clone(),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        )
        .with_core_version(Some("1.20.1".to_string()));

        let mut source = source(server.url.clone());
        source.user_agent = None;
        let report = pipeline
            .update(&source)
            .await
            .expect("subscription should update");

        assert_eq!(report.status, SubscriptionPipelineStatus::Updated);
        assert_eq!(report.display_name.as_deref(), Some("SSRDOG"));
        assert_eq!(report.parsed.proxies[0].name, "alpha");
        assert_eq!(report.metadata.etag.as_deref(), Some("\"etag-a\""));
        let user_info = report
            .metadata
            .last_update
            .as_ref()
            .and_then(|result| result.user_info.as_ref())
            .expect("subscription-userinfo should be persisted");
        assert_eq!(user_info.used_bytes(), Some(1_305_336_542));
        assert_eq!(user_info.total, Some(322_122_547_200));
        assert_eq!(user_info.expire, Some(1_816_327_723_000));
        assert_eq!(
            store.cached_content("sub-a").unwrap(),
            yaml_subscription().as_bytes()
        );
        assert!(contains_header(
            &server.first_request(),
            "user-agent",
            "clash.meta/v1.20.1"
        ));
    }

    #[tokio::test]
    async fn forwards_custom_request_headers_including_user_agent() {
        let server = FakeHttpServer::spawn(vec![FakeResponse {
            status: 200,
            headers: Vec::new(),
            body: yaml_subscription(),
        }]);
        let store = MemoryUpdateStore::default();
        let pipeline = SubscriptionUpdatePipeline::with_client(
            store,
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        );
        let mut source = source(server.url.clone());
        source.user_agent = Some("clash.meta/v2.0.0".to_string());
        source.request_headers = SubscriptionRequestHeaders::new(BTreeMap::from([
            ("User-Agent".to_string(), "custom-agent/1.0".to_string()),
            ("X-Air-Test".to_string(), "header-ok".to_string()),
        ]));

        pipeline
            .update(&source)
            .await
            .expect("request headers should be forwarded");

        let request = server.first_request();
        assert!(contains_header(&request, "user-agent", "clash.meta/v2.0.0"));
        assert!(contains_header(&request, "x-air-test", "header-ok"));
    }

    #[tokio::test]
    async fn default_update_rejects_disabled_source_but_cache_refresh_allows_it() {
        let server = FakeHttpServer::spawn(vec![FakeResponse {
            status: 200,
            headers: Vec::new(),
            body: yaml_subscription(),
        }]);
        let store = MemoryUpdateStore::default();
        let pipeline = SubscriptionUpdatePipeline::with_client(
            store.clone(),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        );
        let mut disabled = source(server.url.clone());
        disabled.enabled = false;

        let error = pipeline
            .update(&disabled)
            .await
            .expect_err("regular update should keep disabled sources out of scheduled refresh");
        assert!(matches!(error, SubscriptionPipelineError::Disabled(_)));

        let report = pipeline
            .update_inactive_cache(&disabled)
            .await
            .expect("manual cache refresh should download disabled source");

        assert_eq!(report.status, SubscriptionPipelineStatus::Updated);
        assert_eq!(
            store.cached_content("sub-a").unwrap(),
            yaml_subscription().as_bytes()
        );
    }

    #[test]
    fn subscription_user_agent_uses_clash_meta_shape() {
        assert_eq!(
            subscription_download_user_agent(Some("v1.19.0")),
            "clash.meta/v1.19.0"
        );
        assert_eq!(subscription_download_user_agent(None), "clash.meta/v0.0.0");
    }

    #[test]
    fn rejects_non_url_proxy_value_except_direct() {
        let mut source = SubscriptionSource::remote(
            "sub-a",
            "Sub A",
            "http://127.0.0.1/sub.yaml?token=secret-token",
        );
        source.proxy = Some("Proxy".to_string());
        let pipeline = SubscriptionUpdatePipeline::with_client(
            MemoryUpdateStore::default(),
            reqwest::Client::new(),
        );

        let error = futures::executor::block_on(pipeline.update(&source))
            .expect_err("unsupported proxy name should fail fast");

        assert!(matches!(
            error,
            SubscriptionPipelineError::InvalidRequest(_)
        ));
        assert!(error.to_string().contains("DIRECT"));
    }

    #[test]
    fn parses_content_disposition_and_subscription_user_info_headers() {
        assert_eq!(
            subscription_name_from_content_disposition(
                "attachment;filename*=UTF-8''%E4%B8%8A%E6%B5%B7.yml"
            )
            .as_deref(),
            Some("上海")
        );
        let info = parse_subscription_user_info(
            "upload=546551043; download=758785499; total=322122547200; expire=1816327723",
        )
        .unwrap();

        assert_eq!(info.used_bytes(), Some(1_305_336_542));
        assert_eq!(info.expire, Some(1_816_327_723_000));
    }

    #[tokio::test]
    async fn sends_conditional_headers_and_reuses_cache_on_304() {
        let server = FakeHttpServer::spawn(vec![FakeResponse {
            status: 304,
            headers: Vec::new(),
            body: "",
        }]);
        let store = MemoryUpdateStore::default();
        store.insert_success("sub-a", yaml_subscription().as_bytes());
        let pipeline = SubscriptionUpdatePipeline::with_client(
            store,
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        );

        let report = pipeline
            .update(&source(server.url.clone()))
            .await
            .expect("304 should reuse cache");

        let request = server.first_request();
        assert_eq!(report.status, SubscriptionPipelineStatus::NotModified);
        assert!(contains_header(&request, "if-none-match", "\"etag-a\""));
        assert!(contains_header(
            &request,
            "if-modified-since",
            "Wed, 21 Oct 2015 07:28:00 GMT"
        ));
        assert_eq!(report.parsed.proxies[0].name, "alpha");
    }

    #[tokio::test]
    async fn network_failure_keeps_previous_success_cache() {
        let store = MemoryUpdateStore::default();
        store.insert_success("sub-a", yaml_subscription().as_bytes());
        let original = store.cached_content("sub-a").unwrap();
        let pipeline = SubscriptionUpdatePipeline::with_client(
            store.clone(),
            reqwest::Client::builder()
                .timeout(Duration::from_millis(300))
                .build()
                .unwrap(),
        );

        let error = pipeline
            .update(&source("http://127.0.0.1:1/sub.yaml".to_string()))
            .await
            .expect_err("connection failure should be reported");

        assert!(matches!(error, SubscriptionPipelineError::Network(_)));
        assert_eq!(store.cached_content("sub-a").unwrap(), original);
        assert!(store.metadata("sub-a").unwrap().last_failure_at.is_some());
    }

    #[tokio::test]
    async fn parse_error_returns_diagnostic_without_leaking_secret_body() {
        let server = FakeHttpServer::spawn(vec![FakeResponse {
            status: 200,
            headers: Vec::new(),
            body: "proxies:\n  - name: bad\n    type: ss\n    password: super-secret\n    port: [",
        }]);
        let store = MemoryUpdateStore::default();
        store.insert_success("sub-a", yaml_subscription().as_bytes());
        let original = store.cached_content("sub-a").unwrap();
        let pipeline = SubscriptionUpdatePipeline::with_client(
            store.clone(),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        );

        let error = pipeline
            .update(&source(server.url.clone()))
            .await
            .expect_err("invalid YAML should fail");
        let rendered = format!("{error:?} {error}");

        assert!(matches!(error, SubscriptionPipelineError::Parse { .. }));
        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains("secret-token"));
        assert_eq!(store.cached_content("sub-a").unwrap(), original);
    }

    #[tokio::test]
    async fn failed_update_preserves_previous_subscription_user_info() {
        let server = FakeHttpServer::spawn(vec![FakeResponse {
            status: 200,
            headers: vec![(
                "Subscription-Userinfo",
                "upload=999; download=888; total=777; expire=666",
            )],
            body: "proxies:\n  - name: bad\n    type: ss\n    port: [",
        }]);
        let store = MemoryUpdateStore::default();
        let previous_user_info = SubscriptionUserInfo {
            upload: Some(10),
            download: Some(20),
            total: Some(100),
            expire: Some(1_816_327_723_000),
        };
        store.insert_success_with_user_info(
            "sub-a",
            yaml_subscription().as_bytes(),
            previous_user_info.clone(),
        );
        let pipeline = SubscriptionUpdatePipeline::with_client(
            store.clone(),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        );

        let error = pipeline
            .update(&source(server.url.clone()))
            .await
            .expect_err("invalid YAML should fail");

        assert!(matches!(error, SubscriptionPipelineError::Parse { .. }));
        let metadata = store.metadata("sub-a").unwrap();
        assert_eq!(
            metadata
                .last_update
                .as_ref()
                .and_then(|result| result.user_info.as_ref()),
            Some(&previous_user_info)
        );
        assert!(metadata.last_failure_at.is_some());
    }

    #[test]
    fn reserves_base64_subscription_parser_interface() {
        let error = SubscriptionParser::default()
            .parse("dm1lc3M6Ly9leGFtcGxlCg==")
            .expect_err("base64 parser is intentionally reserved");

        assert!(matches!(error, SubscriptionPipelineError::Parse { .. }));
        assert!(
            error
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code == "base64-parser-reserved")
        );
    }

    #[test]
    fn invalid_header_value_does_not_leak_secret() {
        let mut source = SubscriptionSource::remote(
            "sub-a",
            "Sub A",
            "http://127.0.0.1/sub.yaml?token=secret-token",
        );
        source.request_headers = SubscriptionRequestHeaders::new(BTreeMap::from([(
            "Authorization".to_string(),
            "secret\r\nvalue".to_string(),
        )]));
        source.url = Some(SubscriptionUrl::new(
            "http://127.0.0.1/sub.yaml?token=secret-token",
        ));
        let pipeline = SubscriptionUpdatePipeline::with_client(
            MemoryUpdateStore::default(),
            reqwest::Client::new(),
        );

        let error = futures::executor::block_on(pipeline.update(&source))
            .expect_err("invalid header should fail before network");
        let rendered = format!("{error:?} {error}");

        assert!(!rendered.contains("secret\r\nvalue"));
        assert!(!rendered.contains("secret-token"));
    }

    fn contains_header(request: &str, name: &str, value: &str) -> bool {
        request.lines().any(|line| {
            let Some((header_name, header_value)) = line.split_once(':') else {
                return false;
            };
            header_name.eq_ignore_ascii_case(name) && header_value.trim() == value
        })
    }
}
