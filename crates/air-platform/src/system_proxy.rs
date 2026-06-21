use std::process::Command;

use air_error::{AppResult, PlatformError};
use serde::{Deserialize, Serialize};

const LOCAL_PROXY_HOST: &str = "127.0.0.1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemProxyUpdate {
    pub services: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemProxyRestore {
    pub restored_services: Vec<String>,
    pub skipped_services: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SystemProxySnapshot {
    pub managed_port: u32,
    pub services: Vec<SystemProxyServiceSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SystemProxyServiceSnapshot {
    pub service: String,
    pub web: MacosProxyState,
    pub secure_web: MacosProxyState,
    pub socks: MacosProxyState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacosProxyState {
    pub enabled: bool,
    pub server: Option<String>,
    pub port: Option<u32>,
    pub authenticated: bool,
}

impl MacosProxyState {
    #[cfg(test)]
    fn local_enabled(port: u32) -> Self {
        Self {
            enabled: true,
            server: Some(LOCAL_PROXY_HOST.into()),
            port: Some(port),
            authenticated: false,
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn matches_local_proxy(&self, port: u32) -> bool {
        self.enabled && self.server.as_deref() == Some(LOCAL_PROXY_HOST) && self.port == Some(port)
    }
}

pub fn enable_local_system_proxy(port: u32) -> AppResult<SystemProxyUpdate> {
    validate_port(port)?;
    platform_enable_local_system_proxy(port)
}

pub fn disable_system_proxy() -> AppResult<SystemProxyUpdate> {
    platform_disable_system_proxy()
}

pub fn capture_system_proxy_snapshot(managed_port: u32) -> AppResult<SystemProxySnapshot> {
    validate_port(managed_port)?;
    platform_capture_system_proxy_snapshot(managed_port)
}

pub fn restore_system_proxy_snapshot(
    snapshot: &SystemProxySnapshot,
) -> AppResult<SystemProxyRestore> {
    validate_port(snapshot.managed_port)?;
    platform_restore_system_proxy_snapshot(snapshot)
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

#[cfg(target_os = "macos")]
fn platform_capture_system_proxy_snapshot(managed_port: u32) -> AppResult<SystemProxySnapshot> {
    let services = macos_network_services()?;
    capture_macos_proxy_snapshot_for_services(managed_port, services)
}

#[cfg(not(target_os = "macos"))]
fn platform_capture_system_proxy_snapshot(_managed_port: u32) -> AppResult<SystemProxySnapshot> {
    Err(PlatformError::Unsupported("当前平台尚未接入系统代理快照".into()).into())
}

#[cfg(target_os = "macos")]
fn platform_restore_system_proxy_snapshot(
    snapshot: &SystemProxySnapshot,
) -> AppResult<SystemProxyRestore> {
    let available_services = macos_network_services()?;
    let restore_services = snapshot
        .services
        .iter()
        .filter(|service| available_services.contains(&service.service))
        .map(|service| service.service.clone())
        .collect::<Vec<_>>();
    let current =
        capture_macos_proxy_snapshot_for_services(snapshot.managed_port, restore_services.clone())?;
    let mut restored_services = Vec::new();
    let mut skipped_services = Vec::new();

    for saved in snapshot
        .services
        .iter()
        .filter(|service| restore_services.contains(&service.service))
    {
        let Some(current_service) = current
            .services
            .iter()
            .find(|service| service.service == saved.service)
        else {
            skipped_services.push(saved.service.clone());
            continue;
        };
        if !service_snapshot_matches_local_proxy(current_service, snapshot.managed_port) {
            skipped_services.push(saved.service.clone());
            continue;
        }
        for args in macos_restore_proxy_args(MacosProxyKind::Web, &saved.service, &saved.web)? {
            run_networksetup_owned(args)?;
        }
        for args in
            macos_restore_proxy_args(MacosProxyKind::SecureWeb, &saved.service, &saved.secure_web)?
        {
            run_networksetup_owned(args)?;
        }
        for args in macos_restore_proxy_args(MacosProxyKind::Socks, &saved.service, &saved.socks)? {
            run_networksetup_owned(args)?;
        }
        restored_services.push(saved.service.clone());
    }

    Ok(SystemProxyRestore {
        restored_services,
        skipped_services,
    })
}

#[cfg(not(target_os = "macos"))]
fn platform_restore_system_proxy_snapshot(
    _snapshot: &SystemProxySnapshot,
) -> AppResult<SystemProxyRestore> {
    Err(PlatformError::Unsupported("当前平台尚未接入系统代理快照恢复".into()).into())
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

#[cfg(target_os = "macos")]
fn run_networksetup_owned(args: Vec<String>) -> AppResult<()> {
    let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_networksetup(&borrowed)
}

#[cfg(target_os = "macos")]
fn run_networksetup_output(args: &[&str]) -> AppResult<String> {
    let output = Command::new("networksetup")
        .args(args)
        .output()
        .map_err(|error| {
            PlatformError::OperationFailed(format!("执行 networksetup 失败: {error}"))
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
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

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MacosProxyKind {
    Web,
    SecureWeb,
    Socks,
}

#[cfg(any(target_os = "macos", test))]
impl MacosProxyKind {
    fn get_command(self) -> &'static str {
        match self {
            Self::Web => "-getwebproxy",
            Self::SecureWeb => "-getsecurewebproxy",
            Self::Socks => "-getsocksfirewallproxy",
        }
    }

    fn set_command(self) -> &'static str {
        match self {
            Self::Web => "-setwebproxy",
            Self::SecureWeb => "-setsecurewebproxy",
            Self::Socks => "-setsocksfirewallproxy",
        }
    }

    fn state_command(self) -> &'static str {
        match self {
            Self::Web => "-setwebproxystate",
            Self::SecureWeb => "-setsecurewebproxystate",
            Self::Socks => "-setsocksfirewallproxystate",
        }
    }
}

#[cfg(target_os = "macos")]
fn capture_macos_proxy_snapshot_for_services(
    managed_port: u32,
    services: Vec<String>,
) -> AppResult<SystemProxySnapshot> {
    let mut service_snapshots = Vec::with_capacity(services.len());
    for service in services {
        let web = read_macos_proxy_state(MacosProxyKind::Web, &service)?;
        let secure_web = read_macos_proxy_state(MacosProxyKind::SecureWeb, &service)?;
        let socks = read_macos_proxy_state(MacosProxyKind::Socks, &service)?;
        service_snapshots.push(SystemProxyServiceSnapshot {
            service,
            web,
            secure_web,
            socks,
        });
    }
    Ok(SystemProxySnapshot {
        managed_port,
        services: service_snapshots,
    })
}

#[cfg(target_os = "macos")]
fn read_macos_proxy_state(kind: MacosProxyKind, service: &str) -> AppResult<MacosProxyState> {
    let output = run_networksetup_output(&[kind.get_command(), service])?;
    parse_macos_proxy_state(&output)
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_proxy_state(output: &str) -> AppResult<MacosProxyState> {
    let mut state = MacosProxyState::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Enabled" => state.enabled = value.eq_ignore_ascii_case("yes"),
            "Server" => {
                state.server = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            "Port" => {
                state.port = if value.is_empty() {
                    None
                } else {
                    Some(value.parse::<u32>().map_err(|error| {
                        PlatformError::OperationFailed(format!(
                            "解析 macOS 系统代理端口失败: {error}"
                        ))
                    })?)
                };
            }
            "Authenticated Proxy Enabled" => {
                state.authenticated = value == "1" || value.eq_ignore_ascii_case("yes");
            }
            _ => {}
        }
    }
    Ok(state)
}

#[cfg(any(target_os = "macos", test))]
fn macos_restore_proxy_args(
    kind: MacosProxyKind,
    service: &str,
    state: &MacosProxyState,
) -> AppResult<Vec<Vec<String>>> {
    let server = state.server.clone().unwrap_or_default();
    let port = state.port.unwrap_or(0);
    validate_port_for_restore(port)?;

    Ok(vec![
        vec![
            kind.set_command().into(),
            service.into(),
            server,
            port.to_string(),
            "off".into(),
        ],
        vec![
            kind.state_command().into(),
            service.into(),
            if state.enabled { "on" } else { "off" }.into(),
        ],
    ])
}

#[cfg(any(target_os = "macos", test))]
fn validate_port_for_restore(port: u32) -> AppResult<()> {
    if port <= u16::MAX as u32 {
        Ok(())
    } else {
        Err(PlatformError::OperationFailed(format!("系统代理端口无效: {port}")).into())
    }
}

#[cfg(any(target_os = "macos", test))]
fn service_snapshot_matches_local_proxy(service: &SystemProxyServiceSnapshot, port: u32) -> bool {
    service.web.matches_local_proxy(port)
        && service.secure_web.matches_local_proxy(port)
        && service.socks.matches_local_proxy(port)
}

#[cfg(test)]
fn snapshot_services_match_local_proxy(
    snapshot: &SystemProxySnapshot,
    services: &[String],
    port: u32,
) -> bool {
    services.iter().all(|service| {
        snapshot
            .services
            .iter()
            .find(|snapshot| &snapshot.service == service)
            .is_some_and(|snapshot| service_snapshot_matches_local_proxy(snapshot, port))
    })
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
    fn macos_get_proxy_output_parses_enabled_server_port_and_auth() {
        let state = parse_macos_proxy_state(
            "Enabled: Yes\nServer: proxy.example.test\nPort: 8080\nAuthenticated Proxy Enabled: 0\n",
        )
        .unwrap();

        assert!(state.enabled);
        assert_eq!(state.server.as_deref(), Some("proxy.example.test"));
        assert_eq!(state.port, Some(8080));
        assert!(!state.authenticated);
    }

    #[test]
    fn macos_restore_args_preserve_disabled_proxy_server_and_port() {
        let state = MacosProxyState {
            enabled: false,
            server: Some("proxy.example.test".into()),
            port: Some(8080),
            authenticated: false,
        };

        let args = macos_restore_proxy_args(MacosProxyKind::Web, "Wi-Fi", &state).unwrap();

        assert_eq!(
            args,
            vec![
                vec![
                    "-setwebproxy".to_string(),
                    "Wi-Fi".to_string(),
                    "proxy.example.test".to_string(),
                    "8080".to_string(),
                    "off".to_string(),
                ],
                vec![
                    "-setwebproxystate".to_string(),
                    "Wi-Fi".to_string(),
                    "off".to_string(),
                ],
            ]
        );
    }

    #[test]
    fn macos_snapshot_matches_only_when_all_proxies_are_air_owned() {
        let current = SystemProxySnapshot {
            managed_port: 9870,
            services: vec![SystemProxyServiceSnapshot {
                service: "Wi-Fi".into(),
                web: MacosProxyState::local_enabled(9870),
                secure_web: MacosProxyState::local_enabled(9870),
                socks: MacosProxyState::local_enabled(9870),
            }],
        };

        assert!(snapshot_services_match_local_proxy(
            &current,
            &["Wi-Fi".to_string()],
            9870
        ));
        assert!(!snapshot_services_match_local_proxy(
            &current,
            &["Wi-Fi".to_string()],
            19090
        ));
    }

    #[test]
    fn system_proxy_port_must_be_valid_tcp_port() {
        assert!(validate_port(9870).is_ok());
        assert!(validate_port(0).is_err());
        assert!(validate_port(70000).is_err());
    }
}
