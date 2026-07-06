extern crate self as air_platform;

// 平台差异统一收口到本模块，业务代码不直接散落 cfg(target_os)。

pub mod autostart;
pub mod core_service;
pub mod elevated_process;
pub mod privilege;
pub mod process;
pub mod process_icon;
pub mod single_instance;
pub mod system_proxy;
pub mod tray;
pub mod window;

use serde::{Deserialize, Serialize};

/// 业务层关心的主机操作系统族。
///
/// 这里不表达具体发行版、内核能力或权限状态；这些执行期能力会在后续
/// `platform::tun` 中细化。本枚举仅供配置校验输出“当前平台可能不支持”的诊断。
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
