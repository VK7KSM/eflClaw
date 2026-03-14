use crate::config::Config;
use anyhow::Result;
use chrono::{Timelike, Utc};
use std::future::Future;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tokio::time::Duration;

const STATUS_FLUSH_SECONDS: u64 = 5;

/// Check whether a TCP port on the given host is already in use.
///
/// Attempts a non-blocking bind; returns `true` if the port is available,
/// `false` if it is already bound by another process.
fn is_port_available(host: &str, port: u16) -> bool {
    use std::net::TcpListener;
    TcpListener::bind(format!("{host}:{port}")).is_ok()
}

pub async fn run(config: Config, host: String, port: u16) -> Result<()> {
    // Early port-conflict detection — gives a clear error before the gateway
    // supervisor starts repeatedly failing and retrying with backoff.
    if !is_port_available(&host, port) {
        anyhow::bail!(
            "Port {port} on {host} is already in use.\n\
             Another ZeroClaw daemon may already be running.\n\
             To check: run `zeroclaw status` or look for an existing daemon process.\n\
             To stop it: run `zeroclaw stop` or kill the process holding port {port}."
        );
    }
    let initial_backoff = config.reliability.channel_initial_backoff_secs.max(1);
    let max_backoff = config
        .reliability
        .channel_max_backoff_secs
        .max(initial_backoff);

    crate::health::mark_component_ok("daemon");

    if config.heartbeat.enabled {
        let _ =
            crate::heartbeat::engine::HeartbeatEngine::ensure_heartbeat_file(&config.workspace_dir)
                .await;
    }

    let mut handles: Vec<JoinHandle<()>> = vec![spawn_state_writer(config.clone())];

    {
        let gateway_cfg = config.clone();
        let gateway_host = host.clone();
        handles.push(spawn_component_supervisor(
            "gateway",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = gateway_cfg.clone();
                let host = gateway_host.clone();
                async move { crate::gateway::run_gateway(&host, port, cfg).await }
            },
        ));
    }

    {
        if has_supervised_channels(&config) {
            let channels_cfg = config.clone();
            handles.push(spawn_component_supervisor(
                "channels",
                initial_backoff,
                max_backoff,
                move || {
                    let cfg = channels_cfg.clone();
                    async move { crate::channels::start_channels(cfg).await }
                },
            ));
        } else {
            crate::health::mark_component_ok("channels");
            tracing::info!("No real-time channels configured; channel supervisor disabled");
        }
    }

    if config.heartbeat.enabled {
        let heartbeat_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "heartbeat",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = heartbeat_cfg.clone();
                async move { Box::pin(run_heartbeat_worker(cfg)).await }
            },
        ));
    }

    if config.cron.enabled {
        let scheduler_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "scheduler",
            initial_backoff,
            max_backoff,
            move || {
                let cfg = scheduler_cfg.clone();
                async move { crate::cron::scheduler::run(cfg).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("scheduler");
        tracing::info!("Cron disabled; scheduler supervisor not started");
    }

    println!("🧠 ZeroClaw daemon started");
    println!("   Gateway:  http://{host}:{port}");
    println!("   Components: gateway, channels, heartbeat, scheduler");
    println!("   Ctrl+C to stop");

    tokio::signal::ctrl_c().await?;
    crate::health::mark_component_error("daemon", "shutdown requested");

    for handle in &handles {
        handle.abort();
    }
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

pub fn state_file_path(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("daemon_state.json")
}

fn spawn_state_writer(config: Config) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = state_file_path(&config);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(STATUS_FLUSH_SECONDS));
        loop {
            interval.tick().await;
            let mut json = crate::health::snapshot_json();
            if let Some(obj) = json.as_object_mut() {
                obj.insert(
                    "written_at".into(),
                    serde_json::json!(Utc::now().to_rfc3339()),
                );
            }
            let data = serde_json::to_vec_pretty(&json).unwrap_or_else(|_| b"{}".to_vec());
            let _ = tokio::fs::write(&path, data).await;
        }
    })
}

fn spawn_component_supervisor<F, Fut>(
    name: &'static str,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    mut run_component: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        loop {
            crate::health::mark_component_ok(name);
            match run_component().await {
                Ok(()) => {
                    crate::health::mark_component_error(name, "component exited unexpectedly");
                    tracing::warn!("Daemon component '{name}' exited unexpectedly");
                    // Clean exit — reset backoff since the component ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    crate::health::mark_component_error(name, e.to_string());
                    tracing::error!("Daemon component '{name}' failed: {e}");
                }
            }

            crate::health::bump_component_restart(name);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

async fn run_heartbeat_worker(config: Config) -> Result<()> {
    let delivery = heartbeat_delivery_target(&config)?;

    let interval_mins = config.heartbeat.interval_minutes.max(5);
    let mut interval = tokio::time::interval(Duration::from_secs(u64::from(interval_mins) * 60));
    interval.tick().await; // consume the instant first tick — first real execution waits full interval

    let heartbeat_path = config.workspace_dir.join("HEARTBEAT.md");

    loop {
        interval.tick().await;

        // ── activeHours: skip outside configured window ──
        let now = chrono::Local::now();
        let current_minutes = now.hour() * 60 + now.minute();
        let start =
            crate::config::parse_hhmm(&config.heartbeat.active_hours_start).unwrap_or(6 * 60 + 30); // fallback 06:30
        let end = crate::config::parse_hhmm(&config.heartbeat.active_hours_end).unwrap_or(23 * 60); // fallback 23:00
        if !crate::config::is_within_active_hours(current_minutes, start, end) {
            tracing::debug!(
                "Heartbeat skipped: outside active hours ({:02}:{:02}, window {}–{})",
                now.hour(),
                now.minute(),
                config.heartbeat.active_hours_start,
                config.heartbeat.active_hours_end
            );
            crate::health::mark_component_ok("heartbeat");
            continue;
        }

        // ── Read entire HEARTBEAT.md ──
        let content = match tokio::fs::read_to_string(&heartbeat_path).await {
            Ok(c) => c,
            Err(_) => {
                tracing::debug!("Heartbeat skipped: HEARTBEAT.md not found");
                crate::health::mark_component_ok("heartbeat");
                continue;
            }
        };

        // ── Skip if effectively empty (only headers/blank lines) ──
        if is_heartbeat_content_empty(&content) {
            tracing::debug!("Heartbeat skipped: HEARTBEAT.md is effectively empty");
            crate::health::mark_component_ok("heartbeat");
            continue;
        }

        // ── Build whole-file prompt (one agent turn) ──
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M %Z");
        // elfClaw: simplified heartbeat — verify cron only, no log diagnostics
        // Log diagnostics are handled by self_check (triggered by user via main model).
        // This keeps heartbeat within weak-model capability and avoids runaway tool loops.
        let prompt = format!(
            "[Heartbeat] 当前时间: {now}\n\n\
             以下是 HEARTBEAT.md 的完整内容：\n\n\
             {content}\n\n\
             请执行以下步骤：\n\
             1. 用 cron_list 检查现有 Cron 任务\n\
             2. 对比 HEARTBEAT.md 中的时间表\n\
             3. 如果所有任务都已存在（名称匹配即可），直接回复 HEARTBEAT_OK\n\
             4. 只有在某个任务**完全不存在于 cron_list 结果中**时，才用 cron_add 创建\n\
                （必须设置 recurring_confirmed=true，delivery 设为 announce 到 telegram:495916105）\n\
             5. 如果任务已存在但时间略有不同，这是正常的，回复 HEARTBEAT_OK\n\n\
             严格规则：\n\
             - 已存在的任务绝对不要用 cron_add 重新创建\n\
             - cron_add 失败后不要重试，回复 HEARTBEAT_OK 即可\n\
             - 不要调用 check_logs、self_check\n\
             - 大多数情况下所有任务都已存在，你只需要回复 HEARTBEAT_OK\n\
             - ⚠️ 时区规则：所有 cron 时间都是悉尼时间（AEST/AEDT）。\
               创建 cron_add 时必须设置 schedule.tz=\"Australia/Sydney\"。\
               绝对不要使用 UTC 时间，HEARTBEAT.md 中写的时间就是悉尼本地时间。"
        );

        let temp = config.default_temperature;
        match crate::agent::run(
            config.clone(),
            Some(prompt),
            None,
            None,
            temp,
            vec![],
            false,
            Some(config.heartbeat.max_tool_iterations),
            crate::agent::RunContext::Background, // elfClaw: heartbeat uses worker_model
            None,                                 // elfClaw: no tool filtering for heartbeat
        )
        .await
        {
            Ok(output) => {
                crate::health::mark_component_ok("heartbeat");

                // ── HEARTBEAT_OK suppression ──
                if contains_heartbeat_ok(&output) {
                    tracing::debug!("Heartbeat: HEARTBEAT_OK — no delivery");
                    continue;
                }

                // Only deliver when the agent has something to report
                if !output.trim().is_empty() {
                    if let Some((channel, target)) = &delivery {
                        if let Err(e) = crate::cron::scheduler::deliver_announcement(
                            &config, channel, target, &output,
                        )
                        .await
                        {
                            crate::health::mark_component_error(
                                "heartbeat",
                                format!("delivery failed: {e}"),
                            );
                            tracing::warn!("Heartbeat delivery failed: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                crate::health::mark_component_error("heartbeat", e.to_string());
                tracing::warn!("Heartbeat task failed: {e}");
                // elfClaw: log heartbeat failure
                crate::elfclaw_log::log_error(
                    "heartbeat",
                    &format!("Heartbeat failed: {e}"),
                    serde_json::json!({}),
                );
            }
        }

        // ── Chat log summarization (silent, never blocks heartbeat) ──
        if config.chat_log.enabled {
            match crate::channels::chat_summarizer::summarize_chat_logs(&config).await {
                Ok(report) if report.processed > 0 => {
                    tracing::info!(
                        "Chat summarization: {} processed, {} skipped, {} errors",
                        report.processed,
                        report.skipped,
                        report.errors.len()
                    );
                }
                Ok(_) => {} // all skipped — nothing to log
                Err(e) => tracing::warn!("Chat summarization failed: {e}"),
            }
        }
    }
}

/// Check whether `HEARTBEAT_OK` appears at the start or end of the agent reply.
/// Matches the OpenClaw suppression contract: the token at start/end means "nothing to report".
fn contains_heartbeat_ok(output: &str) -> bool {
    const TOKEN: &str = "HEARTBEAT_OK";
    let trimmed = output.trim();
    trimmed.starts_with(TOKEN) || trimmed.ends_with(TOKEN)
}

/// A HEARTBEAT.md file is "effectively empty" if every line is blank or a markdown header.
/// In that case we skip the API call to save tokens (mirrors OpenClaw behaviour).
fn is_heartbeat_content_empty(content: &str) -> bool {
    content.lines().all(|line| {
        let t = line.trim();
        t.is_empty() || t.starts_with('#')
    })
}

// Legacy helper retained for existing tests; no longer called from the main heartbeat loop.
fn heartbeat_tasks_for_tick(
    file_tasks: Vec<String>,
    fallback_message: Option<&str>,
) -> Vec<String> {
    if !file_tasks.is_empty() {
        return file_tasks;
    }

    fallback_message
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(|message| vec![message.to_string()])
        .unwrap_or_default()
}

fn heartbeat_delivery_target(config: &Config) -> Result<Option<(String, String)>> {
    let channel = config
        .heartbeat
        .target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let target = config
        .heartbeat
        .to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match (channel, target) {
        (None, None) => Ok(None),
        (Some(_), None) => anyhow::bail!("heartbeat.to is required when heartbeat.target is set"),
        (None, Some(_)) => anyhow::bail!("heartbeat.target is required when heartbeat.to is set"),
        (Some(channel), Some(target)) => {
            validate_heartbeat_channel_config(config, channel)?;
            Ok(Some((channel.to_string(), target.to_string())))
        }
    }
}

fn validate_heartbeat_channel_config(config: &Config, channel: &str) -> Result<()> {
    match channel.to_ascii_lowercase().as_str() {
        "telegram" => {
            if config.channels_config.telegram.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to telegram but channels_config.telegram is not configured"
                );
            }
        }
        "discord" => {
            if config.channels_config.discord.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to discord but channels_config.discord is not configured"
                );
            }
        }
        "slack" => {
            if config.channels_config.slack.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to slack but channels_config.slack is not configured"
                );
            }
        }
        "mattermost" => {
            if config.channels_config.mattermost.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to mattermost but channels_config.mattermost is not configured"
                );
            }
        }
        "whatsapp_web" => {
            match config.channels_config.whatsapp.as_ref() {
                None => anyhow::bail!(
                    "heartbeat.target is set to whatsapp_web but channels_config.whatsapp is not configured"
                ),
                Some(wapp) => {
                    if wapp.access_token.is_some() || wapp.phone_number_id.is_some() {
                        anyhow::bail!(
                            "heartbeat.target is set to whatsapp_web but channels_config.whatsapp is configured for cloud mode (access_token/phone_number_id). Use session_path for Web mode."
                        );
                    }
                }
            }
        }
        other => anyhow::bail!("unsupported heartbeat.target channel: {other}"),
    }

    Ok(())
}

fn has_supervised_channels(config: &Config) -> bool {
    config
        .channels_config
        .channels_except_webhook()
        .iter()
        .any(|(_, ok)| *ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    #[test]
    fn state_file_path_uses_config_directory() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let path = state_file_path(&config);
        assert_eq!(path, tmp.path().join("daemon_state.json"));
    }

    #[tokio::test]
    async fn supervisor_marks_error_and_restart_on_failure() {
        let handle = spawn_component_supervisor("daemon-test-fail", 1, 1, || async {
            anyhow::bail!("boom")
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-fail"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("boom"));
    }

    #[tokio::test]
    async fn supervisor_marks_unexpected_exit_as_error() {
        let handle = spawn_component_supervisor("daemon-test-exit", 1, 1, || async { Ok(()) });

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-exit"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("component exited unexpectedly"));
    }

    #[test]
    fn detects_no_supervised_channels() {
        let config = Config::default();
        assert!(!has_supervised_channels(&config));
    }

    #[test]
    fn detects_supervised_channels_present() {
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "token".into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
            progress_mode: crate::config::ProgressMode::default(),
            ack_enabled: true,
            group_reply: None,
            base_url: None,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_dingtalk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.dingtalk = Some(crate::config::schema::DingTalkConfig {
            client_id: "client_id".into(),
            client_secret: "client_secret".into(),
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_mattermost_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.mattermost = Some(crate::config::schema::MattermostConfig {
            url: "https://mattermost.example.com".into(),
            bot_token: "token".into(),
            channel_id: Some("channel-id".into()),
            allowed_users: vec!["*".into()],
            thread_replies: Some(true),
            mention_only: Some(false),
            group_reply: None,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_qq_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.qq = Some(crate::config::schema::QQConfig {
            app_id: "app-id".into(),
            app_secret: "app-secret".into(),
            allowed_users: vec!["*".into()],
            receive_mode: crate::config::schema::QQReceiveMode::Websocket,
            environment: crate::config::schema::QQEnvironment::Production,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_nextcloud_talk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.nextcloud_talk = Some(crate::config::schema::NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: None,
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn heartbeat_tasks_use_file_tasks_when_available() {
        let tasks =
            heartbeat_tasks_for_tick(vec!["From file".to_string()], Some("Fallback from config"));
        assert_eq!(tasks, vec!["From file".to_string()]);
    }

    #[test]
    fn heartbeat_tasks_fall_back_to_config_message() {
        let tasks = heartbeat_tasks_for_tick(vec![], Some("  check london time  "));
        assert_eq!(tasks, vec!["check london time".to_string()]);
    }

    #[test]
    fn heartbeat_tasks_ignore_empty_fallback_message() {
        let tasks = heartbeat_tasks_for_tick(vec![], Some("   "));
        assert!(tasks.is_empty());
    }

    #[test]
    fn heartbeat_delivery_target_none_when_unset() {
        let config = Config::default();
        let target = heartbeat_delivery_target(&config).unwrap();
        assert!(target.is_none());
    }

    #[test]
    fn heartbeat_delivery_target_requires_to_field() {
        let mut config = Config::default();
        config.heartbeat.target = Some("telegram".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("heartbeat.to is required when heartbeat.target is set"));
    }

    #[test]
    fn heartbeat_delivery_target_requires_target_field() {
        let mut config = Config::default();
        config.heartbeat.to = Some("123456".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("heartbeat.target is required when heartbeat.to is set"));
    }

    #[test]
    fn heartbeat_delivery_target_rejects_unsupported_channel() {
        let mut config = Config::default();
        config.heartbeat.target = Some("email".into());
        config.heartbeat.to = Some("ops@example.com".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported heartbeat.target channel"));
    }

    #[test]
    fn heartbeat_delivery_target_requires_channel_configuration() {
        let mut config = Config::default();
        config.heartbeat.target = Some("telegram".into());
        config.heartbeat.to = Some("123456".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("channels_config.telegram is not configured"));
    }

    #[test]
    fn heartbeat_delivery_target_accepts_telegram_configuration() {
        let mut config = Config::default();
        config.heartbeat.target = Some("telegram".into());
        config.heartbeat.to = Some("123456".into());
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "bot-token".into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
            progress_mode: crate::config::ProgressMode::default(),
            ack_enabled: true,
            group_reply: None,
            base_url: None,
        });

        let target = heartbeat_delivery_target(&config).unwrap();
        assert_eq!(target, Some(("telegram".to_string(), "123456".to_string())));
    }

    #[test]
    fn heartbeat_delivery_target_accepts_whatsapp_web_target_in_web_mode() {
        let mut config = Config::default();
        config.heartbeat.target = Some("whatsapp_web".into());
        config.heartbeat.to = Some("+15551234567".into());
        config.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: None,
            phone_number_id: None,
            verify_token: None,
            app_secret: None,
            session_path: Some("~/.zeroclaw/state/whatsapp-web/session.db".into()),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["*".into()],
        });

        let target = heartbeat_delivery_target(&config).unwrap();
        assert_eq!(
            target,
            Some(("whatsapp_web".to_string(), "+15551234567".to_string()))
        );
    }

    #[test]
    fn heartbeat_delivery_target_rejects_whatsapp_web_target_in_cloud_mode() {
        let mut config = Config::default();
        config.heartbeat.target = Some("whatsapp_web".into());
        config.heartbeat.to = Some("+15551234567".into());
        config.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: Some("token".into()),
            phone_number_id: Some("123456".into()),
            verify_token: Some("verify".into()),
            app_secret: None,
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["*".into()],
        });

        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err.to_string().contains("configured for cloud mode"));
    }

    #[test]
    fn port_available_returns_true_for_unused_port() {
        // Pick an obscure high port unlikely to be in use.
        // is_port_available opens and immediately drops a TcpListener, so
        // repeated calls on the same port work fine.
        assert!(is_port_available("127.0.0.1", 59998));
    }

    #[test]
    fn port_available_returns_false_when_bound() {
        use std::net::TcpListener;
        // Bind a listener to hold the port.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bound_port = listener.local_addr().unwrap().port();

        // While _listener holds the port, is_port_available should return false.
        assert!(!is_port_available("127.0.0.1", bound_port));
        drop(listener);
    }
}
