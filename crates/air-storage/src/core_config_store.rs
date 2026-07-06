use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_yaml::Value;

use air_config::model::MihomoConfigDocument;
use air_config::{ConfigDocument, SubscriptionMergeInput};
use air_error::{AppResult, StorageError};

use super::{AppPaths, FileStore};

pub const CORE_COMMON_CONFIG_PATH: &str = "core.common.config.yaml";
pub const CORE_RUNTIME_CONFIG_PATH: &str = "core.runtime.config.yaml";

#[cfg(not(target_os = "macos"))]
const DEFAULT_TUN_DEVICE: &str = "air";

#[cfg(not(target_os = "macos"))]
const DEFAULT_TUN_AUTO_ROUTE: &str = "false";

const DEFAULT_CORE_CONFIG_PREFIX: &str = r#"mixed-port: 9870
ipv6: false
external-controller: 127.0.0.1:19090
dns:
  enable: true
  ipv6: false
  listen: 0.0.0.0:1053
  enhanced-mode: redir-host
  fake-ip-range: 198.18.0.1/16
  use-hosts: true
  default-nameserver:
    - tls://223.5.5.5
  nameserver:
    - https://doh.pub/dns-query
    - https://dns.alidns.com/dns-query
  fake-ip-filter:
    - "*"
    - +.lan
    - +.local
    - time.*.com
    - ntp.*.com
    - +.market.xiaomi.com
  use-system-hosts: true
tun:
  enable: true
"#;

const DEFAULT_CORE_CONFIG_SUFFIX: &str = r#"  stack: mixed
  auto-redirect: false
  dns-hijack:
    - any:53
  mtu: 1500
sniffer:
  enable: true
  parse-pure-ip: true
  force-dns-mapping: true
  override-destination: false
  sniff:
    HTTP:
      ports:
        - 80
        - 443
      override-destination: false
    TLS:
      ports:
        - 443
  skip-domain:
    - +.push.apple.com
  skip-dst-address:
    - 91.105.192.0/23
    - 91.108.4.0/22
    - 91.108.8.0/21
    - 91.108.16.0/21
    - 91.108.56.0/22
    - 95.161.64.0/20
    - 149.154.160.0/20
    - 185.76.151.0/24
    - 2001:67c:4e8::/48
    - 2001:b28:f23c::/47
    - 2001:b28:f23f::/48
    - 2a0a:f280:203::/48
geo-update-interval: 24
find-process-mode: always
proxies: []
proxy-groups: []
rules: []
"#;

#[derive(Clone, Debug)]
pub struct CoreConfigStore {
    paths: AppPaths,
    files: FileStore,
}

impl CoreConfigStore {
    pub fn new(paths: AppPaths) -> Self {
        let files = FileStore::new(paths.config_dir.clone(), paths.backups_dir.clone());
        Self { paths, files }
    }

    pub fn load_user_config(&self) -> AppResult<ConfigDocument> {
        let target = self.paths.config_dir.join(CORE_COMMON_CONFIG_PATH);
        match fs::read_to_string(&target) {
            Ok(source) => {
                tracing::info!(path = %target.display(), "loading user core config");
                Ok(ConfigDocument::parse(source)?)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tracing::info!(path = %target.display(), "user core config missing; using built-in defaults");
                Ok(ConfigDocument::parse(default_core_config_source())?)
            }
            Err(error) => Err(StorageError::Io(error).into()),
        }
    }

    pub fn save_user_config(&self, document: &ConfigDocument) -> AppResult<()> {
        tracing::info!(
            path = %self.common_config_path().display(),
            proxies = document.typed.proxies.len(),
            proxy_groups = document.typed.proxy_groups.len(),
            rules = document.typed.rules.len(),
            "saving user core config"
        );
        self.write_yaml(Path::new(CORE_COMMON_CONFIG_PATH), &document.typed)
    }

    pub fn ensure_user_config_exists(&self) -> AppResult<ConfigDocument> {
        let target = self.paths.config_dir.join(CORE_COMMON_CONFIG_PATH);
        let document = self.load_user_config()?;
        if !target.exists() {
            self.save_user_config(&document)?;
        }
        Ok(document)
    }

    pub fn write_runtime_config(
        &self,
        base: &MihomoConfigDocument,
        subscriptions: &[SubscriptionMergeInput],
    ) -> AppResult<PathBuf> {
        tracing::info!(
            enabled_subscriptions = subscriptions.iter().filter(|source| source.enabled).count(),
            "writing merged runtime config"
        );
        let runtime = Self::merged_runtime_config(base, subscriptions);
        self.write_runtime_document(&runtime)
    }

    pub fn merged_runtime_config(
        base: &MihomoConfigDocument,
        subscriptions: &[SubscriptionMergeInput],
    ) -> MihomoConfigDocument {
        tracing::info!(
            base_proxies = base.proxies.len(),
            base_proxy_groups = base.proxy_groups.len(),
            base_rules = base.rules.len(),
            enabled_subscriptions = subscriptions.iter().filter(|source| source.enabled).count(),
            "building merged runtime config"
        );
        let mut runtime = base.clone();
        for subscription in subscriptions.iter().filter(|source| source.enabled) {
            tracing::info!(
                subscription_id = %subscription.id,
                subscription_name = %subscription.display_name,
                proxies = subscription.document.proxies.len(),
                proxy_groups = subscription.document.proxy_groups.len(),
                rules = subscription.document.rules.len(),
                "merging subscription into runtime config"
            );
            apply_subscription_sections(&mut runtime, &subscription.document);
        }
        runtime
    }

    pub fn write_runtime_document(&self, document: &MihomoConfigDocument) -> AppResult<PathBuf> {
        let path = Path::new(CORE_RUNTIME_CONFIG_PATH);
        tracing::info!(
            path = %self.runtime_config_path().display(),
            proxies = document.proxies.len(),
            proxy_groups = document.proxy_groups.len(),
            rules = document.rules.len(),
            "writing runtime core config"
        );
        self.write_yaml(path, document)?;
        Ok(self.paths.config_dir.join(path))
    }

    pub fn runtime_config_yaml(document: &MihomoConfigDocument) -> AppResult<String> {
        let mut yaml = serde_yaml::to_value(document).map_err(StorageError::Yaml)?;
        prune_nulls(&mut yaml);
        serde_yaml::to_string(&yaml).map_err(|error| StorageError::Yaml(error).into())
    }

    pub fn runtime_config_path(&self) -> PathBuf {
        self.paths.config_dir.join(CORE_RUNTIME_CONFIG_PATH)
    }

    pub fn common_config_path(&self) -> PathBuf {
        self.paths.config_dir.join(CORE_COMMON_CONFIG_PATH)
    }

    fn write_yaml<T: Serialize>(&self, path: &Path, value: &T) -> AppResult<()> {
        let mut yaml = serde_yaml::to_value(value).map_err(StorageError::Yaml)?;
        prune_nulls(&mut yaml);
        let bytes = serde_yaml::to_string(&yaml)
            .map_err(StorageError::Yaml)?
            .into_bytes();
        // 所有核心配置文件都在配置目录下原子替换，避免 mihomo 启动时读到半写入文件。
        self.files.write_bytes(path, &bytes)
    }
}

fn default_core_config_source() -> String {
    #[cfg(target_os = "macos")]
    {
        format!(
            "{DEFAULT_CORE_CONFIG_PREFIX}  auto-route: true\n  inet4-address:\n    - 172.19.0.1/30\n{DEFAULT_CORE_CONFIG_SUFFIX}"
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!(
            "{DEFAULT_CORE_CONFIG_PREFIX}  device: {DEFAULT_TUN_DEVICE}\n  auto-route: {DEFAULT_TUN_AUTO_ROUTE}\n{DEFAULT_CORE_CONFIG_SUFFIX}"
        )
    }
}

fn apply_subscription_sections(target: &mut MihomoConfigDocument, incoming: &MihomoConfigDocument) {
    for proxy in &incoming.proxies {
        replace_by_name(&mut target.proxies, proxy.clone(), |item| {
            item.name.as_str()
        });
    }
    for group in &incoming.proxy_groups {
        replace_by_name(&mut target.proxy_groups, group.clone(), |item| {
            item.name.as_str()
        });
    }
    target
        .proxy_providers
        .extend(incoming.proxy_providers.clone());
    target
        .rule_providers
        .extend(incoming.rule_providers.clone());
    if !incoming.rules.is_empty() {
        target.rules.extend(incoming.rules.clone());
    }
    target.sub_rules.extend(incoming.sub_rules.clone());
}

fn replace_by_name<T>(items: &mut Vec<T>, next: T, name: impl Fn(&T) -> &str) -> bool {
    let next_name = name(&next).to_string();
    if let Some(existing) = items.iter_mut().find(|item| name(item) == next_name) {
        *existing = next;
        true
    } else {
        items.push(next);
        false
    }
}

fn prune_nulls(value: &mut Value) {
    match value {
        Value::Mapping(map) => {
            let keys = map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let remove = match map.get_mut(&key) {
                    Some(Value::Null) => true,
                    Some(child) => {
                        prune_nulls(child);
                        prune_blank_geox_url_fields(&key, child);
                        is_empty_geox_url(&key, child)
                            || is_runtime_sensitive_empty_field(&key, child)
                    }
                    None => false,
                };
                if remove {
                    map.remove(&key);
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                prune_nulls(item);
            }
        }
        _ => {}
    }
}

fn is_empty_geox_url(key: &Value, child: &Value) -> bool {
    matches!(key, Value::String(key) if key == "geox-url")
        && matches!(child, Value::Mapping(map) if map.is_empty())
}

fn is_runtime_sensitive_empty_field(key: &Value, child: &Value) -> bool {
    let Value::String(key) = key else {
        return false;
    };
    matches!(key.as_str(), "lan-allowed-ips")
        && matches!(child, Value::Sequence(items) if items.is_empty())
}

fn prune_blank_geox_url_fields(key: &Value, child: &mut Value) {
    if !matches!(key, Value::String(key) if key == "geox-url") {
        return;
    }
    let Value::Mapping(map) = child else {
        return;
    };
    for field in ["geoip", "geosite", "mmdb", "asn"] {
        let key = Value::String(field.to_string());
        let remove = matches!(map.get(&key), Some(Value::String(value)) if value.trim().is_empty());
        if remove {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use air_config::model::{GeoxUrlConfig, ProxyGroup, ProxyKind, ProxyNode, RuleLine};

    fn store_in_temp() -> (tempfile::TempDir, CoreConfigStore) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        paths.init().unwrap();
        (temp, CoreConfigStore::new(paths))
    }

    #[test]
    fn saves_core_config_without_yaml_nulls() {
        let (temp, store) = store_in_temp();
        let document = store.load_user_config().unwrap();

        store.save_user_config(&document).unwrap();

        let source =
            fs::read_to_string(temp.path().join("config/core.common.config.yaml")).unwrap();
        assert!(source.contains("mixed-port: 9870"));
        assert!(source.contains("listen: 0.0.0.0:1053"));
        assert!(source.contains("geo-update-interval: 24"));
        assert!(!source.contains("null"));
        assert!(
            !source.contains("lan-allowed-ips: []"),
            "mihomo v1.19.27 treats an empty lan-allowed-ips list as rejecting local mixed-port clients"
        );
    }

    #[test]
    fn preserves_non_empty_lan_allowed_ips_when_saving_core_config() {
        let (temp, store) = store_in_temp();
        let mut document = store.load_user_config().unwrap();
        document.typed.global.lan_allowed_ips = vec!["127.0.0.1/32".to_string()];

        store.save_user_config(&document).unwrap();

        let source =
            fs::read_to_string(temp.path().join("config/core.common.config.yaml")).unwrap();
        assert!(source.contains("lan-allowed-ips:"));
        assert!(source.contains("- 127.0.0.1/32"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_core_config_uses_macos_safe_tun_defaults() {
        let (_temp, store) = store_in_temp();
        let document = store.load_user_config().unwrap();
        let tun = document
            .typed
            .tun
            .expect("默认配置需要包含 TUN 配置，方便首次启动核心");

        assert_eq!(
            tun.device.as_deref(),
            None,
            "macOS 不应固定 utun 设备名，避免和系统已有 utun0/utun1 冲突"
        );
        assert_eq!(tun.auto_route, Some(true));
        assert_ne!(tun.auto_redirect, Some(true));
        assert_eq!(
            tun.inet4_address,
            vec!["172.19.0.1/30".to_string()],
            "macOS TUN 地址不应落入 198.18.0.0/16 fake-ip 池，否则 mixed-port 可能被自路由重置"
        );
        assert_eq!(
            document.typed.global.ipv6,
            Some(false),
            "默认只配置 IPv4 TUN 时应关闭全局 IPv6，避免浏览器用真实 IPv6 绕过 TUN"
        );
        assert_eq!(
            document.typed.dns.as_ref().and_then(|dns| dns.ipv6),
            Some(false),
            "默认只配置 IPv4 TUN 时 DNS 不应返回 AAAA，避免浏览器优先走真实 IPv6"
        );
    }

    #[test]
    fn empty_geox_url_is_removed_when_saving_core_config() {
        let (temp, store) = store_in_temp();
        let mut document = store.load_user_config().unwrap();
        document.typed.global.geox_url = Some(GeoxUrlConfig::default());

        store.save_user_config(&document).unwrap();

        let source =
            fs::read_to_string(temp.path().join("config/core.common.config.yaml")).unwrap();
        assert!(!source.contains("geox-url"));
        assert!(!source.contains("geoip: null"));
        assert!(!source.contains("geosite: null"));
        assert!(!source.contains("mmdb: null"));
        assert!(!source.contains("asn: null"));
    }

    #[test]
    fn empty_geox_url_fields_are_removed_individually_when_saving_core_config() {
        let (temp, store) = store_in_temp();
        let mut document = store.load_user_config().unwrap();
        document.typed.global.geox_url = Some(GeoxUrlConfig {
            geoip: None,
            geosite: Some("https://example.test/geosite.dat".to_string()),
            mmdb: None,
            asn: None,
            ..GeoxUrlConfig::default()
        });

        store.save_user_config(&document).unwrap();

        let source =
            fs::read_to_string(temp.path().join("config/core.common.config.yaml")).unwrap();
        assert!(source.contains("geox-url"));
        assert!(source.contains("geosite: https://example.test/geosite.dat"));
        assert!(!source.contains("geoip:"));
        assert!(!source.contains("mmdb:"));
        assert!(!source.contains("asn:"));
    }

    #[test]
    fn blank_geox_url_fields_are_removed_when_saving_core_config() {
        let (temp, store) = store_in_temp();
        let mut document = store.load_user_config().unwrap();
        document.typed.global.geox_url = Some(GeoxUrlConfig {
            geoip: Some(String::new()),
            geosite: Some("   ".to_string()),
            mmdb: Some("https://example.test/country.mmdb".to_string()),
            asn: Some("\t".to_string()),
            ..GeoxUrlConfig::default()
        });

        store.save_user_config(&document).unwrap();

        let source =
            fs::read_to_string(temp.path().join("config/core.common.config.yaml")).unwrap();
        assert!(source.contains("geox-url"));
        assert!(source.contains("mmdb: https://example.test/country.mmdb"));
        assert!(!source.contains("geoip:"));
        assert!(!source.contains("geosite:"));
        assert!(!source.contains("asn:"));
    }

    #[test]
    fn ensure_user_config_exists_creates_common_config() {
        let (temp, store) = store_in_temp();

        let document = store.ensure_user_config_exists().unwrap();

        assert_eq!(document.typed.global.mixed_port, Some(9870));
        assert_eq!(
            document.typed.global.external_controller.as_deref(),
            Some("127.0.0.1:19090")
        );
        assert_eq!(document.typed.global.geo_update_interval, Some(24));
        assert_eq!(document.typed.global.ipv6, Some(false));
        assert_eq!(
            document.typed.dns.as_ref().and_then(|dns| dns.enable),
            Some(true)
        );
        assert_eq!(
            document.typed.dns.as_ref().and_then(|dns| dns.ipv6),
            Some(false)
        );
        let tun_device = document
            .typed
            .tun
            .as_ref()
            .and_then(|tun| tun.device.as_deref());
        #[cfg(target_os = "macos")]
        assert_eq!(tun_device, None);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(tun_device, Some(DEFAULT_TUN_DEVICE));
        assert_eq!(
            document
                .typed
                .sniffer
                .as_ref()
                .and_then(|sniffer| sniffer.enable),
            Some(true)
        );
        assert!(temp.path().join("config/core.common.config.yaml").exists());
        assert!(!temp.path().join("config/core.config.yaml").exists());
    }

    #[test]
    fn runtime_merge_only_allows_subscription_policy_sections() {
        let (temp, store) = store_in_temp();
        let mut base = store.load_user_config().unwrap().typed;
        base.global.external_controller = Some("127.0.0.1:19090".to_string());

        let mut sub = MihomoConfigDocument::default();
        sub.global.external_controller = Some("0.0.0.0:9090".to_string());
        sub.proxies.push(ProxyNode {
            name: "sub-node".to_string(),
            kind: ProxyKind::Direct,
            ..ProxyNode::default()
        });
        sub.proxy_groups.push(ProxyGroup {
            name: "Proxy".to_string(),
            proxies: vec!["sub-node".to_string()],
            ..ProxyGroup::default()
        });
        sub.rules.push(RuleLine {
            raw: "MATCH,Proxy".to_string(),
        });

        store
            .write_runtime_config(
                &base,
                &[SubscriptionMergeInput {
                    id: "sub".to_string(),
                    display_name: "Sub".to_string(),
                    enabled: true,
                    document: sub,
                }],
            )
            .unwrap();

        let source =
            fs::read_to_string(temp.path().join("config/core.runtime.config.yaml")).unwrap();
        assert!(source.contains("sub-node"));
        assert!(source.contains("127.0.0.1:19090"));
        assert!(!source.contains("0.0.0.0:9090"));
        assert!(!source.contains("null"));
    }
}
