use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use air_error::{AppResult, ProcessError};
use air_mihomo::detect::MihomoRuntimeInfo;
use air_mihomo::release::{
    ArchiveFormat, CoreAsset, CoreReleaseProvider, HostPlatform, choose_asset, install_file_name,
};
use air_telemetry::redaction::redact_log_value;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CoreInstallEvent {
    DownloadStarted { url: String },
    ChecksumVerified { sha256: String },
    Extracted { file_name: String },
    Installed { path: PathBuf },
    LocalBinarySelected { path: PathBuf },
}

#[derive(Clone, Debug)]
pub struct CoreInstallMetadata {
    pub current_version: Option<String>,
    pub target_version: String,
    pub install_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CoreAcquisitionService {
    cores_dir: PathBuf,
    http: reqwest::Client,
    host: HostPlatform,
}

impl CoreAcquisitionService {
    pub fn new(cores_dir: PathBuf) -> Self {
        Self {
            cores_dir,
            http: reqwest::Client::new(),
            host: HostPlatform::current(),
        }
    }

    pub fn with_host(cores_dir: PathBuf, host: HostPlatform) -> Self {
        Self {
            cores_dir,
            http: reqwest::Client::new(),
            host,
        }
    }

    pub fn install_path_for(&self, target_version: &str) -> PathBuf {
        self.cores_dir
            .join(target_version.trim_start_matches('v'))
            .join(self.host.executable_name())
    }

    pub fn select_local_binary(&self, path: PathBuf) -> AppResult<CoreInstallEvent> {
        if !path.is_file() {
            return Err(ProcessError::BinaryNotFound(path).into());
        }
        Ok(CoreInstallEvent::LocalBinarySelected { path })
    }

    pub async fn install_latest<P: CoreReleaseProvider>(
        &self,
        provider: &P,
        current: Option<&MihomoRuntimeInfo>,
    ) -> AppResult<(CoreInstallMetadata, Vec<CoreInstallEvent>)> {
        let release = provider.latest_release(&self.host).await?;
        let asset = choose_asset(&release, &self.host)
            .ok_or_else(|| ProcessError::Install("没有匹配当前平台的 mihomo 发布包".into()))?;
        let install_path = self.install_path_for(&release.version);
        let metadata = CoreInstallMetadata {
            current_version: current.and_then(|info| info.version.clone()),
            target_version: release.version.clone(),
            install_path: install_path.clone(),
        };
        let events = self.download_and_install(asset, &install_path).await?;
        Ok((metadata, events))
    }

    async fn download_and_install(
        &self,
        asset: &CoreAsset,
        install_path: &Path,
    ) -> AppResult<Vec<CoreInstallEvent>> {
        fs::create_dir_all(
            install_path
                .parent()
                .ok_or_else(|| ProcessError::Install("安装路径缺少父目录".into()))?,
        )
        .map_err(ProcessError::Io)?;

        let mut events = vec![CoreInstallEvent::DownloadStarted {
            url: redact_log_value(&asset.download_url),
        }];
        let response = self
            .http
            .get(&asset.download_url)
            .send()
            .await
            .map_err(|error| ProcessError::Install(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProcessError::Install(format!("下载失败，HTTP 状态 {status}")).into());
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ProcessError::Install(error.to_string()))?;
        if let Some(expected) = &asset.sha256 {
            verify_sha256(&bytes, expected)?;
            events.push(CoreInstallEvent::ChecksumVerified {
                sha256: redact_log_value(expected),
            });
        }

        let temp_dir = tempfile::tempdir().map_err(ProcessError::Io)?;
        let archive_path = temp_dir.path().join(&asset.file_name);
        fs::write(&archive_path, &bytes).map_err(ProcessError::Io)?;
        let extracted = extract_asset(asset, &archive_path, temp_dir.path(), &self.host)?;
        events.push(CoreInstallEvent::Extracted {
            file_name: extracted
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("mihomo")
                .to_owned(),
        });
        replace_existing_binary_safely(&extracted, install_path)?;
        set_executable_permission(install_path)?;
        events.push(CoreInstallEvent::Installed {
            path: install_path.to_path_buf(),
        });
        Ok(events)
    }
}

pub fn verify_sha256(bytes: &[u8], expected: &str) -> AppResult<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(ProcessError::Install("下载文件 SHA256 校验失败".into()).into())
    }
}

fn extract_asset(
    asset: &CoreAsset,
    archive_path: &Path,
    output_dir: &Path,
    host: &HostPlatform,
) -> AppResult<PathBuf> {
    match asset.archive {
        ArchiveFormat::Plain => Ok(archive_path.to_path_buf()),
        ArchiveFormat::GzipSingle => {
            let output = output_dir.join(install_file_name(Path::new(&asset.file_name), host));
            let mut decoder =
                GzDecoder::new(fs::File::open(archive_path).map_err(ProcessError::Io)?);
            let mut file = fs::File::create(&output).map_err(ProcessError::Io)?;
            io::copy(&mut decoder, &mut file).map_err(ProcessError::Io)?;
            Ok(output)
        }
        ArchiveFormat::TarGzip => extract_tar_gz(archive_path, output_dir, host),
        ArchiveFormat::Zip => extract_zip(archive_path, output_dir, host),
    }
}

fn extract_tar_gz(
    archive_path: &Path,
    output_dir: &Path,
    host: &HostPlatform,
) -> AppResult<PathBuf> {
    let decoder = GzDecoder::new(fs::File::open(archive_path).map_err(ProcessError::Io)?);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(ProcessError::Io)? {
        let mut entry = entry.map_err(ProcessError::Io)?;
        let path = entry.path().map_err(ProcessError::Io)?.into_owned();
        if path.file_name().and_then(|name| name.to_str()) == Some(host.executable_name()) {
            let output = output_dir.join(host.executable_name());
            entry.unpack(&output).map_err(ProcessError::Io)?;
            return Ok(output);
        }
    }
    Err(ProcessError::Install("发布包中未找到 mihomo 可执行文件".into()).into())
}

fn extract_zip(archive_path: &Path, output_dir: &Path, host: &HostPlatform) -> AppResult<PathBuf> {
    let file = fs::File::open(archive_path).map_err(ProcessError::Io)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| ProcessError::Install(error.to_string()))?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| ProcessError::Install(error.to_string()))?;
        let Some(name) = Path::new(file.name())
            .file_name()
            .and_then(|name| name.to_str())
        else {
            continue;
        };
        if name == host.executable_name() {
            let output = output_dir.join(host.executable_name());
            let mut out = fs::File::create(&output).map_err(ProcessError::Io)?;
            io::copy(&mut file, &mut out).map_err(ProcessError::Io)?;
            return Ok(output);
        }
    }
    Err(ProcessError::Install("发布包中未找到 mihomo 可执行文件".into()).into())
}

fn replace_existing_binary_safely(source: &Path, target: &Path) -> AppResult<()> {
    // 先写入同目录临时文件，再替换目标；若替换阶段失败，会尝试恢复旧文件，避免破坏可用核心。
    let parent = target
        .parent()
        .ok_or_else(|| ProcessError::Install("安装路径缺少父目录".into()))?;
    let temp_path = parent.join(format!(
        ".{}.new",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mihomo")
    ));
    fs::copy(source, &temp_path).map_err(ProcessError::Io)?;
    let backup = parent.join(format!(
        ".{}.old",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mihomo")
    ));
    if target.exists() {
        if backup.exists() {
            fs::remove_file(&backup).map_err(ProcessError::Io)?;
        }
        fs::rename(target, &backup).map_err(ProcessError::Io)?;
    }
    if let Err(error) = fs::rename(&temp_path, target) {
        if backup.exists() {
            let _ = fs::rename(&backup, target);
        }
        return Err(ProcessError::Io(error).into());
    }
    if backup.exists() {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable_permission(path: &Path) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).map_err(ProcessError::Io)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(ProcessError::Io)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable_permission(_path: &Path) -> AppResult<()> {
    // Windows 执行权限由扩展名和 ACL 决定，这里不主动修改用户 ACL。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use air_mihomo::release::{CpuArch, PlatformFamily};

    #[test]
    fn chooses_managed_install_path_by_version_and_platform() {
        let temp = tempfile::tempdir().unwrap();
        let service = CoreAcquisitionService::with_host(
            temp.path().join("cores"),
            HostPlatform {
                family: PlatformFamily::Windows,
                arch: CpuArch::X86_64,
            },
        );

        assert!(
            service
                .install_path_for("v1.19.0")
                .ends_with(Path::new("1.19.0").join("mihomo.exe"))
        );
    }

    #[test]
    fn detects_checksum_mismatch() {
        let error = verify_sha256(b"abc", "deadbeef").unwrap_err();
        assert!(matches!(error, air_error::AppError::Process(_)));
    }

    #[test]
    fn replacement_keeps_existing_file_when_new_copy_fails() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("mihomo");
        fs::write(&target, b"old").unwrap();
        let missing = temp.path().join("missing");

        let _ = replace_existing_binary_safely(&missing, &target);

        assert_eq!(fs::read(&target).unwrap(), b"old");
    }
}
