use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use air_error::AppResult;
pub const CORE_SERVICE_NAME: &str = "AirMihomoCore";
pub const CORE_SERVICE_DISPLAY_NAME: &str = "Air Mihomo Core Service";
pub(super) const CORE_SERVICE_ARG: &str = "--air-mihomo-service";
#[cfg(windows)]
pub(super) const ELEVATED_SERVICE_HELPER_ARG: &str = "--air-elevated-service-helper";
#[cfg(any(windows, test))]
pub(super) const SERVICE_OWNER_PID_ARG: &str = "--owner-pid";
#[cfg(windows)]
pub(super) const SERVICE_ADMIN_RIGHTS_SDDL: &str = "CCDCLCSWRPWPDTLOCRSDRCWDWO";
#[cfg(windows)]
pub(super) const SERVICE_INTERACTIVE_USER_RIGHTS_SDDL: &str = "LCRPWP";

// Windows 标准访问位不属于服务模块本身；这里显式保留数值，避免为了少量 ACL 掩码引入额外
// windows-sys feature。它们分别对应 DELETE / READ_CONTROL / WRITE_DAC / WRITE_OWNER。
#[cfg(windows)]
pub(super) const STANDARD_DELETE: u32 = 0x0001_0000;
#[cfg(windows)]
pub(super) const STANDARD_READ_CONTROL: u32 = 0x0002_0000;
#[cfg(windows)]
pub(super) const STANDARD_WRITE_DAC: u32 = 0x0004_0000;
#[cfg(windows)]
pub(super) const STANDARD_WRITE_OWNER: u32 = 0x0008_0000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CoreServiceSnapshot {
    pub installed: bool,
    pub running: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreServiceAction {
    Install,
    Uninstall,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreServicePaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub cores_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl CoreServicePaths {
    pub fn from_base_dirs(config_dir: &Path, data_dir: &Path, cache_dir: &Path) -> Self {
        Self {
            config_dir: config_dir.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
            cache_dir: cache_dir.to_path_buf(),
            cores_dir: cache_dir.join("core"),
            logs_dir: data_dir.join("logs"),
        }
    }

    pub fn resolve_default() -> AppResult<Self> {
        let project_dirs = directories::ProjectDirs::from("org.air", "", "Air")
            .ok_or(air_error::StorageError::ProjectDirsUnavailable)?;
        Ok(Self::from_base_dirs(
            project_dirs.config_dir(),
            project_dirs.data_dir(),
            project_dirs.cache_dir(),
        ))
    }

    #[cfg(windows)]
    pub(super) fn init(&self) -> AppResult<()> {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.cores_dir,
            &self.logs_dir,
        ] {
            std::fs::create_dir_all(dir).map_err(air_error::StorageError::Io)?;
        }
        Ok(())
    }
}

impl CoreServiceAction {
    #[cfg(windows)]
    pub(super) fn as_arg(self) -> &'static str {
        match self {
            Self::Install => "--install",
            Self::Uninstall => "--uninstall",
        }
    }

    #[cfg(windows)]
    pub(super) fn from_arg(value: &str) -> Option<Self> {
        match value {
            "--install" => Some(Self::Install),
            "--uninstall" => Some(Self::Uninstall),
            _ => None,
        }
    }
}
