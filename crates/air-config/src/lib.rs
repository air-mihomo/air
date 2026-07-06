extern crate self as air_config;

use std::path::Path;

use async_trait::async_trait;

use air_error::AppResult;

pub mod diagnostic;
pub mod dns;
pub mod merge;
pub mod model;
pub mod override_script;
pub mod sniffer;
pub mod subscriptions;
pub mod tun;
pub mod yaml;

pub use diagnostic::{ConfigDiagnostic, ConfigDiagnosticSeverity};
pub use dns::{
    DnsConfigSettings, DnsNameserverPolicySettings, DnsNameserverProtocol, DnsNameserverViewModel,
    DnsPolicyValueStyle,
};
pub use merge::{
    ConfigMergeChange, ConfigMergeChangeKind, ConfigMergeInput, ConfigMergeOverrides,
    ConfigMergePipeline, ConfigMergePreview, ConfigMergeRuntimePaths, ConfigMergeStage,
    ConfigMergeSummary, ConfigMergeWriteReport, OverrideRuleMode, SubscriptionMergeInput,
    preview_config_merge, write_merged_config,
};
pub use model::MihomoConfigDocument;
pub use override_script::{DEFAULT_OVERRIDE_SCRIPT, apply_override_script};
pub use platform_kind::{PlatformKind, current_platform_kind};
pub use sniffer::{SnifferConfigSettings, SnifferProtocolSettings};
pub use subscriptions::*;
pub use tun::TunConfigSettings;
pub use yaml::{ConfigDocument, YamlConfigDocumentService};

#[async_trait]
pub trait ConfigDocumentService: Send + Sync {
    // 配置文档服务必须保留原始 YAML 树，后续 schema 未覆盖字段也能往返写回。
    async fn load(&self, path: &Path) -> AppResult<ConfigDocument>;
    async fn save(&self, path: &Path, document: &ConfigDocument) -> AppResult<()>;
}

mod platform_kind {
    use serde::{Deserialize, Serialize};

    /// 配置层只需要平台族用于诊断输出，不依赖平台实现 crate，避免 config -> platform 的反向依赖。
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum PlatformKind {
        Windows,
        Macos,
        Linux,
        Android,
        Unknown,
    }

    pub fn current_platform_kind() -> PlatformKind {
        match std::env::consts::OS {
            "windows" => PlatformKind::Windows,
            "macos" => PlatformKind::Macos,
            "linux" => PlatformKind::Linux,
            "android" => PlatformKind::Android,
            _ => PlatformKind::Unknown,
        }
    }
}
