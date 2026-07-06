use std::sync::Arc;
use std::time::Instant;

use air_app::command::{AppCommand, CommandId, CommandResult};
use air_app::runtime::CancellationToken;
use air_app::services::AppServices;
use air_error::AppResult;
use air_telemetry::redaction::redact_log_value;

mod cancellation;
mod config;
mod connections;
mod context;
mod core;
mod monitoring;
mod proxy;
mod rules;
mod shared;
mod subscriptions;

use cancellation::{CommandCancellationRegistry, long_task_key};
use context::CommandExecutionContext;

#[cfg(test)]
use monitoring::core_log_line_to_stream_event;

#[derive(Clone)]
pub struct AppCommandRouter {
    services: Arc<AppServices>,
    cancellations: CommandCancellationRegistry,
}

impl AppCommandRouter {
    pub fn new(services: AppServices) -> Self {
        Self {
            services: Arc::new(services),
            cancellations: CommandCancellationRegistry::default(),
        }
    }

    pub fn services(&self) -> Arc<AppServices> {
        Arc::clone(&self.services)
    }

    pub fn dispatch(&self, command: AppCommand) -> CommandId {
        let id = self.services.runtime.next_command_id();
        let kind = command.kind();
        tracing::info!(
            command_id = id.0,
            command_kind = kind,
            command_payload = %command.log_payload(),
            "dispatching app command"
        );
        let token = CancellationToken::new();
        let cleanup_key = self.register_long_task(&command, token.clone());
        let services = Arc::clone(&self.services);
        let cancellations = self.cancellations.clone();
        let cleanup_cancellations = self.cancellations.clone();
        let command_for_task = command.clone();
        let task_token = token.clone();
        let cleanup_token = token.clone();
        let future = async move {
            let started_at = Instant::now();
            let outcome = execute_command(
                Arc::clone(&services),
                cancellations,
                token,
                command_for_task.clone(),
            )
            .await;
            if let Some(key) = cleanup_key.as_deref() {
                cleanup_cancellations.remove_if_same(key, &cleanup_token);
            }
            let elapsed_ms = started_at.elapsed().as_millis();
            match &outcome {
                Ok(()) => {
                    tracing::info!(
                        command_id = id.0,
                        command_kind = command_for_task.kind(),
                        elapsed_ms,
                        "app command completed"
                    );
                    services.snapshots.clear_last_error();
                }
                Err(error) => {
                    tracing::warn!(
                        command_id = id.0,
                        command_kind = command_for_task.kind(),
                        elapsed_ms,
                        error = %redact_log_value(&error.to_string()),
                        "app command failed"
                    );
                    services.snapshots.set_last_error(error.to_string());
                }
            }
            match outcome {
                Ok(()) => CommandResult::ok(id, command_for_task),
                Err(error) => CommandResult::failed(id, command_for_task, error),
            }
        };
        let _task = self
            .services
            .runtime
            .spawn_command_with_token(id, command, task_token, future);
        id
    }

    fn register_long_task(&self, command: &AppCommand, token: CancellationToken) -> Option<String> {
        let key = long_task_key(command)?;
        self.cancellations.insert(key.clone(), token);
        Some(key)
    }

    pub fn cancel_registered(&self, command: &AppCommand) -> bool {
        long_task_key(command)
            .and_then(|key| self.cancellations.cancel(&key))
            .is_some()
    }
}

async fn execute_command(
    services: Arc<AppServices>,
    cancellations: CommandCancellationRegistry,
    token: CancellationToken,
    command: AppCommand,
) -> AppResult<()> {
    // 命令路由位于 app 层：UI 只提交用户意图，domain/storage/core/api 只暴露稳定能力；
    // 只有 app 层同时知道运行时、仓储、外部进程和事件总线，因此适合承担编排和错误脱敏边界。
    let kind = command.kind();
    tracing::info!(
        command_kind = kind,
        command_payload = %command.log_payload(),
        "executing app command"
    );
    let context = CommandExecutionContext::new(services, cancellations, token);
    match command {
        AppCommand::DetectCore => core::handle_detect_core(&context).await,
        AppCommand::PrepareCore => core::handle_prepare_core(&context).await,
        AppCommand::StartCore => core::handle_start_core(&context).await,
        AppCommand::StopCore => core::handle_stop_core(&context).await,
        AppCommand::RestartCore => core::handle_restart_core(&context).await,
        AppCommand::RefreshCoreService => core::handle_refresh_core_service(&context).await,
        AppCommand::InstallCoreService => core::handle_install_core_service(&context).await,
        AppCommand::UninstallCoreService => core::handle_uninstall_core_service(&context).await,
        AppCommand::StartLogMonitoring => monitoring::handle_start_log_monitoring(&context).await,
        AppCommand::StopLogMonitoring => monitoring::handle_stop_log_monitoring(&context).await,
        AppCommand::StartTrafficMonitoring => {
            monitoring::handle_start_traffic_monitoring(&context).await
        }
        AppCommand::StopTrafficMonitoring => {
            monitoring::handle_stop_traffic_monitoring(&context).await
        }
        AppCommand::StartConnectionsMonitoring => {
            monitoring::handle_start_connections_monitoring(&context).await
        }
        AppCommand::StopConnectionsMonitoring => {
            monitoring::handle_stop_connections_monitoring(&context).await
        }
        AppCommand::SetRuntimeMode { mode } => proxy::handle_set_runtime_mode(&context, mode).await,
        AppCommand::RefreshProxies => proxy::handle_refresh_proxies(&context).await,
        AppCommand::SelectProxy { group, proxy } => {
            proxy::handle_select_proxy(&context, group, proxy).await
        }
        AppCommand::TestProxyDelay {
            name,
            url: _,
            timeout_ms,
        } => proxy::handle_test_proxy_delay(&context, name, timeout_ms).await,
        AppCommand::TestProxyGroupDelay {
            name,
            url: _,
            timeout_ms,
        } => proxy::handle_test_proxy_group_delay(&context, name, timeout_ms).await,
        AppCommand::ClearProxyGroupFixed { name } => {
            proxy::handle_clear_proxy_group_fixed(&context, name).await
        }
        AppCommand::RefreshRules => rules::handle_refresh_rules(&context).await,
        AppCommand::DisableRule { index, disabled } => {
            rules::handle_disable_rule(&context, index, disabled).await
        }
        AppCommand::UpdateRuleProvider { name } => {
            rules::handle_update_rule_provider(&context, name).await
        }
        AppCommand::UpdateSubscription { subscription_id } => {
            subscriptions::handle_update_subscription(&context, subscription_id).await
        }
        AppCommand::RefreshDueSubscriptions => {
            subscriptions::handle_refresh_due_subscriptions(&context).await
        }
        AppCommand::SaveSubscriptionSource { source } => {
            subscriptions::handle_save_subscription_source(&context, source).await
        }
        AppCommand::LoadSubscriptionYaml { subscription_id } => {
            subscriptions::handle_load_subscription_yaml(&context, subscription_id).await
        }
        AppCommand::ReorderSubscriptions { ordered_ids } => {
            subscriptions::handle_reorder_subscriptions(&context, ordered_ids).await
        }
        AppCommand::SelectSubscription { subscription_id } => {
            subscriptions::handle_select_subscription(&context, subscription_id).await
        }
        AppCommand::ImportSubscriptionUrl {
            subscription_id,
            url,
        } => subscriptions::handle_import_subscription_url(&context, subscription_id, url).await,
        AppCommand::ImportSubscriptionFile { path } => {
            subscriptions::handle_import_subscription_file(&context, path).await
        }
        AppCommand::DeleteSubscription { subscription_id } => {
            subscriptions::handle_delete_subscription(&context, subscription_id).await
        }
        AppCommand::CancelSubscriptionUpdate { subscription_id } => {
            subscriptions::handle_cancel_subscription_update(&context, subscription_id).await
        }
        AppCommand::LoadProfile { path } => config::handle_load_profile(&context, path).await,
        AppCommand::SaveConfig { profile } => config::handle_save_config(&context, profile).await,
        AppCommand::SetOverrideScriptEnabled { enabled } => {
            config::handle_set_override_script_enabled(&context, enabled).await
        }
        AppCommand::SaveOverrideScript { script, enabled } => {
            config::handle_save_override_script(&context, script, enabled).await
        }
        AppCommand::DebugOverrideScript { script } => {
            config::handle_debug_override_script(&context, script).await
        }
        AppCommand::RefreshConnections => connections::handle_refresh_connections(&context).await,
        AppCommand::CloseConnection { id } => {
            connections::handle_close_connection(&context, id).await
        }
        AppCommand::CloseAllConnections => {
            connections::handle_close_all_connections(&context).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;
    use air_app::{AppEvent, RuntimeStatus};
    use air_mihomo::subscriptions::{
        SubscriptionSource, SubscriptionUpdateOutcome, SubscriptionUpdateResult,
    };
    use air_storage::AppPaths;

    fn router_in_temp() -> (tempfile::TempDir, AppCommandRouter) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base_dirs(
            &temp.path().join("config"),
            &temp.path().join("data"),
            &temp.path().join("cache"),
        );
        let services = AppServices::with_paths(paths).unwrap();
        (temp, AppCommandRouter::new(services))
    }

    #[test]
    fn successful_real_command_emits_lifecycle_events() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();
        router
            .services()
            .snapshots
            .set_last_error("previous controller error");

        let id = router.dispatch(AppCommand::DetectCore);
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandStarted { id: started } if *started == id)
        }));
        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandFinished { id: finished } if *finished == id)
        }));
        assert_eq!(router.services().snapshots.snapshot().last_error, None);
    }

    #[test]
    fn disable_rule_is_noop_when_runtime_is_not_running() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::DisableRule {
            index: 1,
            disabled: true,
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandFinished { id: finished } if *finished == id)
        }));
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
    }

    #[test]
    fn long_running_command_can_be_canceled_by_matching_command() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let start_id = router.dispatch(AppCommand::StartLogMonitoring);
        wait_for_started(&mut events, start_id);
        assert!(router.cancellations.contains("log-monitoring"));

        let stop_id = router.dispatch(AppCommand::StopLogMonitoring);
        let stop_events = collect_until_finished(&mut events, stop_id);
        let start_events = collect_until_finished(&mut events, start_id);

        assert!(
            stop_events.iter().any(|event| {
                matches!(event, AppEvent::CommandFinished { id } if *id == stop_id)
            })
        );
        assert!(
            start_events.iter().any(|event| {
                matches!(event, AppEvent::CommandFinished { id } if *id == start_id)
            })
        );
    }

    #[test]
    fn log_monitoring_dispatches_events_from_core_log() {
        let (_temp, router) = router_in_temp();
        let log_path = router.services().paths.logs_dir.join("core.log");
        std::fs::write(&log_path, "[info] controller ready token=abc secret=def\n").unwrap();
        let mut events = router.services().runtime.subscribe();

        let start_id = router.dispatch(AppCommand::StartLogMonitoring);
        wait_for_started(&mut events, start_id);
        let stream_events = collect_monitoring_stream_events(&mut events);

        assert!(stream_events.iter().any(|event| {
            matches!(
                event,
                air_mihomo::StreamEvent::Log { message, .. }
                    if message.contains("controller ready")
                        && message.contains("token=***")
                        && message.contains("secret=***")
            )
        }));
        let stop_id = router.dispatch(AppCommand::StopLogMonitoring);
        let _ = collect_until_finished(&mut events, stop_id);
        let _ = collect_until_finished(&mut events, start_id);
    }

    #[test]
    fn traffic_monitoring_dispatches_stream_events_from_controller() {
        let server = MonitoringStreamServer::spawn();
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let start_id = router.dispatch(AppCommand::StartTrafficMonitoring);
        wait_for_started(&mut events, start_id);
        let stream_events = collect_monitoring_stream_events(&mut events);

        assert!(stream_events.iter().any(|event| {
            matches!(
                event,
                air_mihomo::StreamEvent::Traffic {
                    upload: 12,
                    download: 34
                }
            )
        }));
        let stop_id = router.dispatch(AppCommand::StopTrafficMonitoring);
        let _ = collect_until_finished(&mut events, stop_id);
        let _ = collect_until_finished(&mut events, start_id);
    }

    #[test]
    fn connections_monitoring_can_be_canceled_by_matching_command() {
        let server = MonitoringStreamServer::spawn();
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let start_id = router.dispatch(AppCommand::StartConnectionsMonitoring);
        wait_for_started(&mut events, start_id);
        assert!(router.cancellations.contains("connections-monitoring"));

        let stop_id = router.dispatch(AppCommand::StopConnectionsMonitoring);
        let _ = collect_until_finished(&mut events, stop_id);
        let _ = collect_until_finished(&mut events, start_id);
        assert!(!router.cancellations.contains("connections-monitoring"));
    }

    #[test]
    fn failing_real_command_emits_visible_error() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::UpdateSubscription {
            subscription_id: "missing".into(),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(
            received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
    }

    #[test]
    fn failed_subscription_update_cleans_cancellation_token() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::UpdateSubscription {
            subscription_id: "missing".into(),
        });
        let _ = collect_until_finished(&mut events, id);

        assert!(!router.cancellations.contains("subscription:missing"));
    }

    #[test]
    fn empty_due_subscription_refresh_cleans_cancellation_token() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RefreshDueSubscriptions);
        let _ = collect_until_finished(&mut events, id);

        assert!(!router.cancellations.contains("subscriptions-scheduler"));
    }

    #[test]
    fn refresh_proxies_emits_proxy_group_projection() {
        let server = GroupApiServer::spawn();
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        let mut source =
            SubscriptionSource::remote("work", "Work", "https://example.test/sub.yaml");
        source.enabled = true;
        router
            .services()
            .subscription_store
            .save_source(source)
            .unwrap();
        router
            .services()
            .subscription_store
            .record_update_result(
                "work",
                SubscriptionUpdateResult::success(1000, 128),
                Some(
                    b"proxies:\n  - name: ss1\n    type: ss\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies:\n      - ss1\n      - DIRECT\n",
                ),
            )
            .unwrap();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RefreshProxies);
        let received = collect_until_finished(&mut events, id);

        let projection = received
            .iter()
            .find_map(|event| match event {
                AppEvent::ProxyGroupStateChanged(projection) => Some(projection),
                _ => None,
            })
            .expect("proxy refresh should emit runtime projection");
        let proxy = projection
            .states
            .iter()
            .find(|state| state.group_name == "Proxy")
            .expect("Proxy group should be projected");
        assert_eq!(proxy.selected.as_deref(), Some("DIRECT"));
        assert!(proxy.members.iter().any(|member| member.name == "ss1"));
        assert_eq!(
            proxy
                .members
                .iter()
                .find(|member| member.name == "ss1")
                .map(|member| member.history.len()),
            Some(1)
        );
    }

    #[test]
    fn refresh_proxies_uses_runtime_proxies_endpoint_with_subscription_order() {
        let server = GroupApiServer::spawn_with_bodies(
            r#"{"mode":"rule"}"#,
            r#"{"proxies":{"Base":{"name":"Base","all":["DIRECT"],"now":"DIRECT","type":"Selector","history":[]},"Active":{"name":"Active","all":["node-a","DIRECT"],"now":"node-a","type":"Selector","history":[]},"Backup":{"name":"Backup","all":["node-b","DIRECT"],"now":"node-b","type":"Selector","history":[]},"RuntimeOnly":{"name":"RuntimeOnly","all":["DIRECT"],"now":"DIRECT","type":"Selector","history":[]},"node-a":{"name":"node-a","type":"Shadowsocks","history":[{"time":"2026-05-29T10:00:00+08:00","delay":166}]},"node-b":{"name":"node-b","type":"Shadowsocks","history":[]}}}"#,
        );
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!(
                "external-controller: {}\nproxy-groups:\n  - name: Base\n    type: select\n    proxies:\n      - DIRECT\n",
                server.base_addr
            ))
            .unwrap();

        let mut active =
            SubscriptionSource::remote("active", "Active", "https://example.test/active.yaml");
        active.enabled = true;
        router
            .services()
            .subscription_store
            .save_source(active)
            .unwrap();
        router
            .services()
            .subscription_store
            .record_update_result(
                "active",
                SubscriptionUpdateResult::success(1000, 128),
                Some(
                    b"proxies:\n  - name: node-a\n    type: ss\nproxy-groups:\n  - name: Active\n    type: select\n    proxies:\n      - node-a\n      - DIRECT\n",
                ),
            )
            .unwrap();

        let mut backup =
            SubscriptionSource::remote("backup", "Backup", "https://example.test/backup.yaml");
        backup.enabled = false;
        router
            .services()
            .subscription_store
            .save_source(backup)
            .unwrap();
        router
            .services()
            .subscription_store
            .record_update_result(
                "backup",
                SubscriptionUpdateResult::success(1000, 128),
                Some(
                    b"proxies:\n  - name: node-b\n    type: ss\nproxy-groups:\n  - name: Backup\n    type: select\n    proxies:\n      - node-b\n      - DIRECT\n",
                ),
            )
            .unwrap();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RefreshProxies);
        let received = collect_until_finished(&mut events, id);

        let projection = received
            .iter()
            .find_map(|event| match event {
                AppEvent::ProxyGroupStateChanged(projection) => Some(projection),
                _ => None,
            })
            .expect("proxy refresh should emit runtime projection");
        let names = projection
            .states
            .iter()
            .map(|state| state.group_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Active", "Backup", "Base", "RuntimeOnly"]);
        assert_eq!(projection.states[0].selected.as_deref(), Some("node-a"));
    }

    #[test]
    fn refresh_proxies_global_mode_only_keeps_global_group() {
        let server = GroupApiServer::spawn_with_bodies(
            r#"{"mode":"global"}"#,
            r#"{"proxies":{"Proxy":{"name":"Proxy","all":["node-a","DIRECT"],"now":"node-a","type":"Selector","history":[]},"GLOBAL":{"name":"GLOBAL","all":["node-a","DIRECT"],"now":"node-a","type":"Selector","history":[]},"node-a":{"name":"node-a","type":"Shadowsocks","history":[]}}}"#,
        );
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!(
                "external-controller: {}\nmode: rule\n",
                server.base_addr
            ))
            .unwrap();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RefreshProxies);
        let received = collect_until_finished(&mut events, id);

        let projection = received
            .iter()
            .find_map(|event| match event {
                AppEvent::ProxyGroupStateChanged(projection) => Some(projection),
                _ => None,
            })
            .expect("proxy refresh should emit runtime projection");
        let names = projection
            .states
            .iter()
            .map(|state| state.group_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["GLOBAL"]);
        assert_eq!(projection.states[0].selected.as_deref(), Some("node-a"));
    }

    #[test]
    fn proxy_delay_uses_configured_app_probe_url() {
        let server = DelayApiServer::spawn();
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        let mut settings = router.services().settings_store.load().unwrap();
        settings.proxy_delay_test_url = "https://probe.example.test/generate_204".into();
        router.services().settings_store.save(&settings).unwrap();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::TestProxyDelay {
            name: "ss1".into(),
            url: "https://ignored.example.test/old".into(),
            timeout_ms: 3000,
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::ProxyDelayMeasured { name, delay_ms }
                    if name == "ss1" && *delay_ms == 123
            )
        }));
        let request = server
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("delay request should be recorded");
        assert!(
            request.starts_with("GET /proxies/ss1/delay?"),
            "unexpected delay request: {request}"
        );
        assert!(
            request.contains("url=https%3A%2F%2Fprobe.example.test%2Fgenerate_204"),
            "delay request should use app settings url: {request}"
        );
        assert!(
            !request.contains("ignored.example.test"),
            "delay request should not use command fallback url: {request}"
        );
    }

    #[test]
    fn refresh_connections_emits_connections_state() {
        let server = ConnectionApiServer::spawn(vec![(
            "GET /connections ",
            r#"{"connections":[{"id":"abc","metadata":{"host":"example.test","destinationPort":"443","network":"tcp","process":"Code.exe"},"chains":["Proxy"],"upload":10,"download":20,"uploadSpeed":1,"downloadSpeed":2,"start":"2026-05-22T10:00:00+08:00"}],"uploadTotal":10,"downloadTotal":20}"#,
        )]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RefreshConnections);
        let received = collect_until_finished(&mut events, id);

        let response = received
            .iter()
            .find_map(|event| match event {
                AppEvent::ConnectionsStateChanged(response) => Some(response),
                _ => None,
            })
            .expect("connections refresh should emit response event");
        assert_eq!(response.connections.len(), 1);
        assert_eq!(response.upload_total, 10);
        assert_eq!(response.download_total, 20);
    }

    #[test]
    fn refresh_rules_emits_runtime_rules_state() {
        let server = ConnectionApiServer::spawn(vec![(
            "GET /rules ",
            r#"{"rules":[{"index":0,"type":"DomainSuffix","payload":"example.com","proxy":"Proxy","extra":{"disabled":false}}]}"#,
        )]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RefreshRules);
        let received = collect_until_finished(&mut events, id);

        let response = received
            .iter()
            .find_map(|event| match event {
                AppEvent::RulesStateChanged(response) => Some(response),
                _ => None,
            })
            .expect("rules refresh should emit response event");
        assert_eq!(response.rules.len(), 1);
        assert_eq!(response.rules[0]["payload"], "example.com");
    }

    #[test]
    fn disabling_rule_patches_runtime_and_refreshes_rules() {
        let server = ConnectionApiServer::spawn(vec![
            ("PATCH /rules/disable ", "{}"),
            (
                "GET /rules ",
                r#"{"rules":[{"index":0,"type":"Match","payload":"","proxy":"DIRECT","extra":{"disabled":true}}]}"#,
            ),
        ]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::DisableRule {
            index: 0,
            disabled: true,
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::RulesStateChanged(response)
                    if response.rules[0]["extra"]["disabled"] == serde_json::json!(true)
            )
        }));
    }

    #[test]
    fn refresh_connections_is_noop_when_runtime_is_not_running() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RefreshConnections);
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandFinished { id: finished } if *finished == id)
        }));
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::ConnectionsStateChanged(_)))
        );
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
    }

    #[test]
    fn core_log_tail_line_is_redacted_for_monitoring_event() {
        let event = core_log_line_to_stream_event(
            "[stderr] fetch https://example.test/sub?token=abc secret=def",
        )
        .expect("core log line should become monitor log event");

        assert!(matches!(
            event,
            air_mihomo::StreamEvent::Log { level, message }
                if level == "error"
                    && message.contains("token=***")
                    && !message.contains("abc")
                    && !message.contains("def")
        ));
    }

    #[test]
    fn core_log_tail_ignores_managed_timestamp_prefix() {
        let event =
            core_log_line_to_stream_event("[2026-06-02T12:34:56+08:00][stderr] controller failed")
                .expect("timestamped core log line should become monitor log event");

        assert!(matches!(
            event,
            air_mihomo::StreamEvent::Log { level, message }
                if level == "error" && message == "controller failed"
        ));
    }

    #[test]
    fn set_runtime_mode_running_patches_api_and_persists_common_config() {
        let server = ConnectionApiServer::spawn(vec![
            ("PATCH /configs ", "{}"),
            ("GET /configs ", r#"{"mode":"direct"}"#),
            (
                "GET /proxies ",
                r#"{"proxies":{"Proxy":{"name":"Proxy","all":["DIRECT"],"now":"DIRECT","type":"Selector","history":[]}}}"#,
            ),
        ]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!(
                "mode: rule\nexternal-controller: {}\n",
                server.base_addr
            ))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::SetRuntimeMode {
            mode: "direct".into(),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandFinished { id: finished } if *finished == id)
        }));
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
        assert_eq!(
            router
                .services()
                .core_config_store
                .load_user_config()
                .unwrap()
                .typed
                .global
                .mode
                .as_deref(),
            Some("direct")
        );
    }

    #[test]
    fn set_runtime_mode_running_refreshes_groups_with_new_mode() {
        let server = ConnectionApiServer::spawn(vec![
            ("PATCH /configs ", "{}"),
            // 刚 PATCH 后 /configs 可能仍短暂返回旧模式；刷新代理组必须使用用户刚选择的目标模式。
            ("GET /configs ", r#"{"mode":"rule"}"#),
            (
                "GET /proxies ",
                r#"{"proxies":{"Proxy":{"name":"Proxy","all":["node-a","DIRECT"],"now":"node-a","type":"Selector","history":[]},"GLOBAL":{"name":"GLOBAL","all":["node-a","DIRECT"],"now":"node-a","type":"Selector","history":[]},"node-a":{"name":"node-a","type":"Shadowsocks","history":[]}}}"#,
            ),
        ]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!(
                "mode: rule\nexternal-controller: {}\n",
                server.base_addr
            ))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::SetRuntimeMode {
            mode: "global".into(),
        });
        let received = collect_until_finished(&mut events, id);

        let projection = received
            .iter()
            .find_map(|event| match event {
                AppEvent::ProxyGroupStateChanged(projection) => Some(projection),
                _ => None,
            })
            .expect("runtime mode switch should refresh proxy groups");
        let names = projection
            .states
            .iter()
            .map(|state| state.group_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["GLOBAL"]);
    }

    #[test]
    fn restart_core_running_uses_mihomo_restart_api() {
        let server = ConnectionApiServer::spawn(vec![("POST /restart ", r#"{"status":"ok"}"#)]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RestartCore);
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandFinished { id: finished } if *finished == id)
        }));
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
        assert!(
            router
                .services()
                .core_config_store
                .runtime_config_path()
                .is_file()
        );
    }

    #[test]
    fn restart_core_running_tun_config_avoids_fast_restart_api() {
        let server = ConnectionApiServer::spawn_with_request_sender();
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!(
                "external-controller: {}\ntun:\n  enable: true\n",
                server.base_addr
            ))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::RestartCore);
        let received = collect_until_finished(&mut events, id);

        assert!(
            received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. })),
            "missing core binary should fail before a controller /restart request is made"
        );
        assert!(
            server
                .requests
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "TUN restart must not use mihomo's non-elevated /restart endpoint"
        );
    }

    #[test]
    fn closing_connection_refreshes_connections_state() {
        let server = ConnectionApiServer::spawn(vec![
            ("DELETE /connections/abc ", "{}"),
            ("GET /connections ", r#"{"connections":[]}"#),
        ]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::CloseConnection { id: "abc".into() });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::ConnectionsStateChanged(response) if response.connections.is_empty())
        }));
    }

    #[test]
    fn selecting_subscription_dispatches_real_state_projection() {
        let (_temp, router) = router_in_temp();
        router
            .services()
            .subscription_store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();
        router
            .services()
            .subscription_store
            .save_source(SubscriptionSource::remote(
                "backup",
                "Backup",
                "https://backup.example.test/sub",
            ))
            .unwrap();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::SelectSubscription {
            subscription_id: "backup".into(),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::SubscriptionStateChanged(projection)
                    if projection.active_subscription_id.as_deref() == Some("backup")
            )
        }));
        assert!(
            router
                .services()
                .subscription_store
                .load_sources()
                .unwrap()
                .iter()
                .find(|source| source.id == "backup")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn selecting_subscription_running_reloads_runtime_config() {
        let server = ConnectionApiServer::spawn(vec![
            ("PUT /configs ", "{}"),
            ("GET /configs ", r#"{"mode":"rule"}"#),
            ("GET /proxies ", r#"{"proxies":{}}"#),
        ]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .save_current_config(&format!("external-controller: {}\n", server.base_addr))
            .unwrap();
        router
            .services()
            .subscription_store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();
        router
            .services()
            .subscription_store
            .save_source(SubscriptionSource::remote(
                "backup",
                "Backup",
                "https://backup.example.test/sub",
            ))
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::SelectSubscription {
            subscription_id: "backup".into(),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::SubscriptionStateChanged(projection)
                    if projection.active_subscription_id.as_deref() == Some("backup")
            )
        }));
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
        assert!(
            router
                .services()
                .core_config_store
                .runtime_config_path()
                .is_file()
        );
    }

    #[test]
    fn save_config_running_reloads_runtime_config() {
        let server = ConnectionApiServer::spawn(vec![("PUT /configs ", "{}")]);
        let (_temp, router) = router_in_temp();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::SaveConfig {
            profile: format!(
                "mixed-port: 19090\nexternal-controller: {}\n",
                server.base_addr
            ),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandFinished { id: finished } if *finished == id)
        }));
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
        let runtime_config =
            std::fs::read_to_string(router.services().core_config_store.runtime_config_path())
                .unwrap();
        assert!(runtime_config.contains("mixed-port: 19090"));
    }

    #[test]
    fn debug_override_script_emits_preview_without_writing_runtime_config() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::DebugOverrideScript {
            script: "function override(_, config) { config['mixed-port'] = 18080; return config; }"
                .into(),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::OverridePreviewGenerated { contents } if contents.contains("mixed-port: 18080")
            )
        }));
        assert!(
            !router
                .services()
                .core_config_store
                .runtime_config_path()
                .exists()
        );
    }

    #[test]
    fn saving_active_override_script_persists_and_rewrites_runtime_config() {
        let (_temp, router) = router_in_temp();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::SaveOverrideScript {
            script: "function override(_, config) { config['mixed-port'] = 18181; return config; }"
                .into(),
            enabled: true,
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(event, AppEvent::CommandFinished { id: finished } if *finished == id)
        }));
        assert!(
            router
                .services()
                .settings_store
                .load()
                .unwrap()
                .override_script_enabled
        );
        let runtime_config =
            std::fs::read_to_string(router.services().core_config_store.runtime_config_path())
                .unwrap();
        assert!(runtime_config.contains("mixed-port: 18181"));
    }

    #[test]
    fn deleting_subscription_persists_to_store_and_emits_projection() {
        let (_temp, router) = router_in_temp();
        router
            .services()
            .subscription_store
            .save_source(SubscriptionSource::remote(
                "work",
                "Work",
                "https://example.test/sub",
            ))
            .unwrap();
        router
            .services()
            .subscription_store
            .record_update_result(
                "work",
                SubscriptionUpdateResult::imported(1, b"proxies: []\n".len() as u64),
                Some(b"proxies: []\n"),
            )
            .unwrap();
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::DeleteSubscription {
            subscription_id: "work".into(),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::SubscriptionStateChanged(projection) if projection.sources.is_empty()
            )
        }));
        assert!(
            router
                .services()
                .subscription_store
                .load_sources()
                .unwrap()
                .is_empty()
        );
        assert!(
            router
                .services()
                .subscription_store
                .cache_metadata("work")
                .unwrap()
                .is_none()
        );
        assert!(
            router
                .services()
                .subscription_store
                .read_cached_content("work")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn updating_disabled_subscription_refreshes_cache_without_runtime_reload() {
        let (_temp, router) = router_in_temp();
        let server = SubscriptionDownloadServer::spawn("proxies: []\n");
        let mut source = SubscriptionSource::remote("work", "Work", server.url);
        source.enabled = false;
        router
            .services()
            .subscription_store
            .save_source(source)
            .unwrap();
        router
            .services()
            .snapshots
            .set_runtime_status(RuntimeStatus::Running);
        let mut events = router.services().runtime.subscribe();

        let id = router.dispatch(AppCommand::UpdateSubscription {
            subscription_id: "work".into(),
        });
        let received = collect_until_finished(&mut events, id);

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::SubscriptionStateChanged(projection)
                    if projection
                        .sources
                        .iter()
                        .any(|source| source.id == "work" && !source.enabled)
            )
        }));
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, AppEvent::UserVisibleError { .. }))
        );
        assert_eq!(
            router
                .services()
                .subscription_store
                .read_cached_content("work")
                .unwrap()
                .as_deref(),
            Some(b"proxies: []\n".as_slice())
        );
        assert!(
            !router
                .services()
                .core_config_store
                .runtime_config_path()
                .exists(),
            "manual subscription refresh must not rewrite runtime config or reload mihomo"
        );
    }

    #[test]
    fn canceling_subscription_update_records_canceled_state() {
        let (_temp, router) = router_in_temp();
        let server = SlowSubscriptionServer::spawn();
        router
            .services()
            .subscription_store
            .save_source(SubscriptionSource::remote("slow", "Slow", server.url))
            .unwrap();
        let mut events = router.services().runtime.subscribe();

        let update_id = router.dispatch(AppCommand::UpdateSubscription {
            subscription_id: "slow".into(),
        });
        wait_for_started(&mut events, update_id);
        assert!(router.cancellations.contains("subscription:slow"));

        router.dispatch(AppCommand::CancelSubscriptionUpdate {
            subscription_id: "slow".into(),
        });
        let received = collect_until_finished(&mut events, update_id);
        let cache = router
            .services()
            .subscription_store
            .cache_metadata("slow")
            .unwrap()
            .unwrap();

        assert!(received.iter().any(|event| {
            matches!(
                event,
                AppEvent::SubscriptionUpdateCanceled { subscription_id } if subscription_id == "slow"
            )
        }));
        assert_eq!(
            cache.last_update.map(|result| result.outcome),
            Some(SubscriptionUpdateOutcome::Canceled)
        );
        assert!(!router.cancellations.contains("subscription:slow"));
    }

    struct SlowSubscriptionServer {
        url: String,
    }

    struct SubscriptionDownloadServer {
        url: String,
    }

    impl SubscriptionDownloadServer {
        fn spawn(body: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("fake request should arrive");
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            });
            Self {
                url: format!("http://{addr}/sub.yaml"),
            }
        }
    }

    impl SlowSubscriptionServer {
        fn spawn() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("fake request should arrive");
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                thread::sleep(Duration::from_secs(2));
                let body = "proxies: []\n";
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            });
            Self {
                url: format!("http://{addr}/slow.yaml"),
            }
        }
    }

    struct MonitoringStreamServer {
        base_addr: String,
    }

    struct GroupApiServer {
        base_addr: String,
    }

    struct DelayApiServer {
        base_addr: String,
        requests: mpsc::Receiver<String>,
    }

    struct ConnectionApiServer {
        base_addr: String,
    }

    struct RecordingConnectionApiServer {
        base_addr: String,
        requests: mpsc::Receiver<String>,
    }

    impl ConnectionApiServer {
        fn spawn(requests: Vec<(&'static str, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            thread::spawn(move || {
                for (expected_prefix, body) in requests {
                    let (mut stream, _) = listener
                        .accept()
                        .expect("fake connection request should arrive");
                    let mut buffer = [0_u8; 2048];
                    let read = stream.read(&mut buffer).unwrap_or_default();
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    assert!(
                        request.starts_with(expected_prefix),
                        "unexpected connections API request: {request}"
                    );
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(headers.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                }
            });
            Self {
                base_addr: format!("127.0.0.1:{}", addr.port()),
            }
        }

        fn spawn_with_request_sender() -> RecordingConnectionApiServer {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            let (sender, requests) = mpsc::channel();
            thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buffer = [0_u8; 2048];
                    let read = stream.read(&mut buffer).unwrap_or_default();
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let _ = sender.send(request);
                    let body = r#"{"status":"ok"}"#;
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(headers.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                }
            });
            RecordingConnectionApiServer {
                base_addr: format!("127.0.0.1:{}", addr.port()),
                requests,
            }
        }
    }

    impl GroupApiServer {
        fn spawn() -> Self {
            Self::spawn_with_bodies(
                r#"{"mode":"rule"}"#,
                r#"{"proxies":{"Proxy":{"name":"Proxy","all":["ss1","DIRECT"],"now":"DIRECT","type":"Selector","history":[]},"ss1":{"name":"ss1","type":"Shadowsocks","history":[{"time":"2026-05-29T10:00:00+08:00","delay":166}]},"DIRECT":{"name":"DIRECT","type":"Direct","history":[]}}}"#,
            )
        }

        fn spawn_with_bodies(configs_body: &'static str, proxies_body: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            thread::spawn(move || {
                for (expected_prefix, body) in [
                    ("GET /configs ", configs_body),
                    ("GET /proxies ", proxies_body),
                ] {
                    let (mut stream, _) =
                        listener.accept().expect("fake group request should arrive");
                    let mut buffer = [0_u8; 4096];
                    let read = stream.read(&mut buffer).unwrap_or_default();
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    assert!(
                        request.starts_with(expected_prefix),
                        "unexpected group API request: {request}"
                    );
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(headers.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                }
            });
            Self {
                base_addr: format!("127.0.0.1:{}", addr.port()),
            }
        }
    }

    impl DelayApiServer {
        fn spawn() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            let (sender, requests) = mpsc::channel();
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("fake delay request should arrive");
                let mut buffer = [0_u8; 2048];
                let read = stream.read(&mut buffer).unwrap_or_default();
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let _ = sender.send(request);
                let body = r#"{"delay":123}"#;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            });
            Self {
                base_addr: format!("127.0.0.1:{}", addr.port()),
                requests,
            }
        }
    }

    impl MonitoringStreamServer {
        fn spawn() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake server should bind");
            let addr = listener
                .local_addr()
                .expect("fake server addr should exist");
            thread::spawn(move || {
                for _ in 0..3 {
                    let (mut stream, _) = listener.accept().expect("fake stream should connect");
                    let mut buffer = [0_u8; 1024];
                    let read = stream.read(&mut buffer).unwrap_or_default();
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let body = if request.starts_with("GET /logs") {
                        r#"{"type":"info","payload":"controller ready"}"#.to_string()
                    } else if request.starts_with("GET /traffic") {
                        r#"{"up":12,"down":34}"#.to_string()
                    } else if request.starts_with("GET /memory") {
                        r#"{"inuse":4096,"oslimit":8192}"#.to_string()
                    } else {
                        r#"{"type":"error","payload":"unexpected path"}"#.to_string()
                    };
                    let body = format!("{body}\n");
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(headers.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                }
            });
            Self {
                base_addr: format!("127.0.0.1:{}", addr.port()),
            }
        }
    }

    fn wait_for_started(
        events: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        command_id: CommandId,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(AppEvent::CommandStarted { id }) if id == command_id => return,
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Closed) => panic!("event channel closed"),
            }
        }
        panic!("timed out waiting for command start");
    }

    fn collect_until_finished(
        events: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        command_id: CommandId,
    ) -> Vec<AppEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(event) => {
                    let finished = matches!(
                        event,
                        AppEvent::CommandFinished { id } if id == command_id
                    );
                    received.push(event);
                    if finished {
                        return received;
                    }
                }
                Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Closed) => panic!("event channel closed"),
            }
        }
        panic!("timed out waiting for command finish");
    }

    fn collect_monitoring_stream_events(
        events: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    ) -> Vec<air_mihomo::StreamEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(AppEvent::MihomoStreamEvent(event)) => {
                    received.push(event);
                    let has_log = received.iter().any(|event| {
                        matches!(
                            event,
                            air_mihomo::StreamEvent::Log { message, .. }
                                if message.contains("controller ready")
                        )
                    });
                    let has_traffic = received.iter().any(|event| {
                        matches!(
                            event,
                            air_mihomo::StreamEvent::Traffic {
                                upload: 12,
                                download: 34
                            }
                        )
                    });
                    if has_log || has_traffic {
                        return received;
                    }
                }
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Closed) => panic!("event channel closed"),
            }
        }
        panic!("timed out waiting for stream events");
    }
}
