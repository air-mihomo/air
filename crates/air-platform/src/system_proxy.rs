use std::process::Command;

use air_error::{AppResult, PlatformError};

const LOCAL_PROXY_HOST: &str = "127.0.0.1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemProxyUpdate {
    pub services: Vec<String>,
}

pub fn enable_local_system_proxy(port: u32) -> AppResult<SystemProxyUpdate> {
    validate_port(port)?;
    platform_enable_local_system_proxy(port)
}

pub fn disable_system_proxy() -> AppResult<SystemProxyUpdate> {
    platform_disable_system_proxy()
}

#[cfg(target_os = "macos")]
fn platform_enable_local_system_proxy(port: u32) -> AppResult<SystemProxyUpdate> {
    let services = macos_network_services()?;
    for service in &services {
        run_networksetup(&[
            "-setwebproxy",
            service,
            LOCAL_PROXY_HOST,
            &port.to_string(),
            "off",
        ])?;
        run_networksetup(&[
            "-setsecurewebproxy",
            service,
            LOCAL_PROXY_HOST,
            &port.to_string(),
            "off",
        ])?;
        run_networksetup(&[
            "-setsocksfirewallproxy",
            service,
            LOCAL_PROXY_HOST,
            &port.to_string(),
            "off",
        ])?;
        run_networksetup(&["-setwebproxystate", service, "on"])?;
        run_networksetup(&["-setsecurewebproxystate", service, "on"])?;
        run_networksetup(&["-setsocksfirewallproxystate", service, "on"])?;
    }
    Ok(SystemProxyUpdate { services })
}

#[cfg(not(target_os = "macos"))]
fn platform_enable_local_system_proxy(_port: u32) -> AppResult<SystemProxyUpdate> {
    Err(PlatformError::Unsupported("当前平台尚未接入系统代理同步".into()).into())
}

#[cfg(target_os = "macos")]
fn platform_disable_system_proxy() -> AppResult<SystemProxyUpdate> {
    let services = macos_network_services()?;
    for service in &services {
        run_networksetup(&["-setwebproxystate", service, "off"])?;
        run_networksetup(&["-setsecurewebproxystate", service, "off"])?;
        run_networksetup(&["-setsocksfirewallproxystate", service, "off"])?;
    }
    Ok(SystemProxyUpdate { services })
}

#[cfg(not(target_os = "macos"))]
fn platform_disable_system_proxy() -> AppResult<SystemProxyUpdate> {
    Err(PlatformError::Unsupported("当前平台尚未接入系统代理同步".into()).into())
}

fn validate_port(port: u32) -> AppResult<()> {
    if (1..=u16::MAX as u32).contains(&port) {
        Ok(())
    } else {
        Err(PlatformError::OperationFailed(format!("系统代理端口无效: {port}")).into())
    }
}

#[cfg(target_os = "macos")]
fn macos_network_services() -> AppResult<Vec<String>> {
    let output = Command::new("networksetup")
        .arg("-listallnetworkservices")
        .output()
        .map_err(|error| {
            PlatformError::OperationFailed(format!("读取 macOS 网络服务失败: {error}"))
        })?;
    if !output.status.success() {
        return Err(PlatformError::OperationFailed(format!(
            "读取 macOS 网络服务失败，networksetup={:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(parse_macos_network_services(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(target_os = "macos")]
fn run_networksetup(args: &[&str]) -> AppResult<()> {
    let output = Command::new("networksetup")
        .args(args)
        .output()
        .map_err(|error| {
            PlatformError::OperationFailed(format!("执行 networksetup 失败: {error}"))
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(PlatformError::OperationFailed(format!(
        "执行 networksetup {:?} 失败，状态 {:?}: {}",
        args,
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    ))
    .into())
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_network_services(output: &str) -> Vec<String> {
    output
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('*'))
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
fn macos_enable_networksetup_args(service: &str, port: u32) -> Vec<Vec<String>> {
    vec![
        vec![
            "-setwebproxy".into(),
            service.into(),
            LOCAL_PROXY_HOST.into(),
            port.to_string(),
            "off".into(),
        ],
        vec![
            "-setsecurewebproxy".into(),
            service.into(),
            LOCAL_PROXY_HOST.into(),
            port.to_string(),
            "off".into(),
        ],
        vec![
            "-setsocksfirewallproxy".into(),
            service.into(),
            LOCAL_PROXY_HOST.into(),
            port.to_string(),
            "off".into(),
        ],
        vec!["-setwebproxystate".into(), service.into(), "on".into()],
        vec![
            "-setsecurewebproxystate".into(),
            service.into(),
            "on".into(),
        ],
        vec![
            "-setsocksfirewallproxystate".into(),
            service.into(),
            "on".into(),
        ],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_network_services_skip_header_disabled_and_keep_spaces() {
        let services = parse_macos_network_services(
            "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\n*iPhone USB\nUSB 10/100/1000 LAN\n\n",
        );

        assert_eq!(
            services,
            vec!["Wi-Fi".to_string(), "USB 10/100/1000 LAN".to_string()]
        );
    }

    #[test]
    fn macos_enable_args_set_all_proxy_kinds_to_local_port() {
        let args = macos_enable_networksetup_args("iPhone USB", 9870);

        assert!(args.contains(&vec![
            "-setwebproxy".into(),
            "iPhone USB".into(),
            "127.0.0.1".into(),
            "9870".into(),
            "off".into(),
        ]));
        assert!(args.contains(&vec![
            "-setsecurewebproxy".into(),
            "iPhone USB".into(),
            "127.0.0.1".into(),
            "9870".into(),
            "off".into(),
        ]));
        assert!(args.contains(&vec![
            "-setsocksfirewallproxy".into(),
            "iPhone USB".into(),
            "127.0.0.1".into(),
            "9870".into(),
            "off".into(),
        ]));
    }

    #[test]
    fn system_proxy_port_must_be_valid_tcp_port() {
        assert!(validate_port(9870).is_ok());
        assert!(validate_port(0).is_err());
        assert!(validate_port(70000).is_err());
    }
}
