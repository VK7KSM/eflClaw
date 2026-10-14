//! Client for cctv-server's `/cctv/v1/` API (contract:
//! `C:\Dev\CCTV\docs\elfclaw-api.md`, owned by the cctv project). Only
//! elfClaw → cctv; the bearer token comes from `CCTV_ELFCLAW_TOKEN`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

pub const TOKEN_ENV: &str = "CCTV_ELFCLAW_TOKEN";

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Preset {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Arm {
    pub state: String,
    #[serde(default)]
    pub until: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Monitor {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub ptz: bool,
    #[serde(default)]
    pub presets: Vec<Preset>,
    #[serde(default)]
    pub arm: Option<Arm>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Clip {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One entry of the event stream or the event query. Fields the contract
/// does not promise for every kind are optional; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Event {
    #[serde(default)]
    pub seq: Option<i64>,
    #[serde(default)]
    pub event_id: Option<i64>,
    /// Set on the `clip_ready` of a clip someone asked for.
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub level: u8,
    #[serde(default)]
    pub monitor_id: Option<u32>,
    #[serde(default)]
    pub monitor_name: Option<String>,
    #[serde(default)]
    pub cause: Option<String>,
    #[serde(default)]
    pub cause_zh: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub end: Option<String>,
    /// `from`/`to` of a requested clip.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub snapshot_url: Option<String>,
    #[serde(default)]
    pub clip: Option<Clip>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub recovery_of: Option<i64>,
    #[serde(default)]
    pub intrusion_confirmed: bool,
    #[serde(default)]
    pub offline_cameras: Option<u32>,
    /// A test alarm (`POST /test/alarm`, v1.4): negative id, not in the
    /// event database, never reported as delivered.
    #[serde(default)]
    pub test: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamPage {
    pub cursor: i64,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub gap: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventPage {
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub next_before: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CctvApi {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl CctvApi {
    /// `base` like `http://192.168.2.21:8095/cctv/v1`.
    pub fn new(base: &str) -> Result<Self> {
        let token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .with_context(|| format!("{TOKEN_ENV} is not set"))?;
        Ok(Self {
            base: base.trim().trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            http: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(5))
                .build()?,
        })
    }

    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            base: "http://127.0.0.1:9/cctv/v1".into(),
            token: "t".into(),
            http: reqwest::Client::new(),
        }
    }

    /// Absolute URL for a path the API handed out (`/cctv/v1/...`) or one
    /// relative to the base.
    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            return path.to_string();
        }
        if let Some(rest) = path.strip_prefix("/cctv/v1") {
            return format!("{}{rest}", self.base);
        }
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<T> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .timeout(timeout)
            .send()
            .await
            .context("cctv-server unreachable")?;
        Self::check(resp)
            .await?
            .json()
            .await
            .context("bad cctv-server reply")
    }

    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(20))
            .json(body)
            .send()
            .await
            .context("cctv-server unreachable")?;
        Self::check(resp)
            .await?
            .json()
            .await
            .context("bad cctv-server reply")
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| body.chars().take(200).collect());
        bail!("cctv-server {}: {message}", status.as_u16())
    }

    /// Long poll. `cursor: None` returns the current cursor and no events.
    pub async fn stream(&self, cursor: Option<i64>, wait_secs: u64) -> Result<StreamPage> {
        let path = match cursor {
            Some(c) => format!("events/stream?cursor={c}&wait={wait_secs}&limit=50"),
            None => format!("events/stream?wait={wait_secs}&limit=50"),
        };
        self.get_json(&path, Duration::from_secs(wait_secs + 15))
            .await
    }

    pub async fn health(&self) -> Result<serde_json::Value> {
        self.get_json("health", Duration::from_secs(10)).await
    }

    pub async fn monitors(&self) -> Result<Vec<Monitor>> {
        #[derive(Deserialize)]
        struct M {
            monitors: Vec<Monitor>,
        }
        Ok(self
            .get_json::<M>("monitors", Duration::from_secs(10))
            .await?
            .monitors)
    }

    /// One page of history (newest first). `query` is the raw query string.
    pub async fn events(&self, query: &str) -> Result<EventPage> {
        self.get_json(&format!("events?{query}"), Duration::from_secs(20))
            .await
    }

    /// One event, in its latest state.
    pub async fn event(&self, id: i64) -> Result<Event> {
        self.get_json(&format!("events/{id}"), Duration::from_secs(15))
            .await
    }

    /// Bytes of a snapshot or clip URL handed out by the API. `Ok(None)`
    /// while a clip is still being made (202).
    pub async fn bytes(&self, path: &str, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .timeout(timeout)
            .send()
            .await
            .context("cctv-server unreachable")?;
        if resp.status().as_u16() == 202 {
            return Ok(None);
        }
        Ok(Some(Self::check(resp).await?.bytes().await?.to_vec()))
    }

    pub async fn live_snapshot(&self, monitor: u32) -> Result<Vec<u8>> {
        self.bytes(
            &format!("monitors/{monitor}/snapshot.jpg"),
            Duration::from_secs(15),
        )
        .await?
        .context("no snapshot")
    }

    /// Record `seconds` (5–20) of live video; returns the job id.
    pub async fn clip_live(&self, monitor: u32, seconds: u32) -> Result<String> {
        let v: serde_json::Value = self
            .post_json(
                &format!("monitors/{monitor}/clip"),
                &serde_json::json!({"seconds": seconds}),
            )
            .await?;
        v["job_id"]
            .as_str()
            .map(str::to_string)
            .context("no job id")
    }

    /// Recorded video from the NVR (≤ 5 min, ending ≥ 2 min ago); job id.
    pub async fn clip_history(&self, monitor: u32, from: &str, to: &str) -> Result<String> {
        let v: serde_json::Value = self
            .post_json(
                "clips",
                &serde_json::json!({"monitor_id": monitor, "from": from, "to": to}),
            )
            .await?;
        v["job_id"]
            .as_str()
            .map(str::to_string)
            .context("no job id")
    }

    pub async fn ptz(&self, body: &serde_json::Value) -> Result<()> {
        let _: serde_json::Value = self.post_json("ptz", body).await?;
        Ok(())
    }

    pub async fn arm_status(&self) -> Result<serde_json::Value> {
        self.get_json("arm", Duration::from_secs(10)).await
    }

    pub async fn set_arm(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.post_json("arm", body).await
    }

    /// Greet at a camera (v1.4): `hello` plays a chime and a spoken English
    /// greeting on its speaker, `flash` blinks HI in Morse on its light.
    pub async fn greet(&self, monitor: u32, mode: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                "greet",
                &serde_json::json!({"monitor_id": monitor, "mode": mode}),
            )
            .await?;
        Ok(())
    }

    /// Tell cctv-server when the Telegram push went out (v1.4, for its
    /// end-to-end latency statistics).
    pub async fn delivered(&self, event_id: i64, at: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url(&format!("events/{event_id}/delivered")))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .json(&serde_json::json!({"channel": "telegram", "at": at}))
            .send()
            .await
            .context("cctv-server unreachable")?;
        Self::check(resp).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_events_and_ignores_unknown_fields() {
        let json = r#"{"events":[{"ai":{"animals":0},"cause":"Intrusion","cause_zh":"区域入侵",
            "clip":{"duration":15.0,"size":7560791,"status":"ready","url":"/cctv/v1/events/33081/clip.mp4"},
            "description":"1 辆车在动","end":"2026-10-14T14:47:06+11:00","event_id":33081,
            "intrusion_confirmed":false,"kind":"alarm","level":2,"monitor_id":3,"monitor_name":"Front yard PTZ",
            "notes":"区域入侵","recovery_of":null,"rule_level":2,"snapshot_url":"/cctv/v1/events/33081/snapshot.jpg",
            "start":"2026-10-14T14:47:06+11:00","target":null,"web_url":"https://cctv.example/#/events/33081"}],
            "next_before":33081}"#;
        let page: EventPage = serde_json::from_str(json).unwrap();
        let e = &page.events[0];
        assert_eq!(
            (e.event_id, e.level, e.kind.as_str()),
            (Some(33081), 2, "alarm")
        );
        assert_eq!(e.clip.as_ref().unwrap().status, "ready");
        assert_eq!(e.description.as_deref(), Some("1 辆车在动"));
        let s: StreamPage =
            serde_json::from_str(r#"{"cursor":10240,"events":[],"gap":true}"#).unwrap();
        assert!(s.gap && s.events.is_empty());
        let job: Event = serde_json::from_str(
            r#"{"seq":5,"kind":"clip_ready","job_id":"c_1","monitor_id":7,"from":"a","to":"b","clip":{"status":"failed","reason":"NVR 没有录像"}}"#,
        )
        .unwrap();
        assert_eq!(job.job_id.as_deref(), Some("c_1"));
        assert_eq!(job.level, 0);
    }

    #[test]
    fn urls_handed_out_by_the_api_resolve_against_the_base() {
        let api = CctvApi {
            base: "http://192.168.2.21:8095/cctv/v1".into(),
            token: "t".into(),
            http: reqwest::Client::new(),
        };
        assert_eq!(
            api.url("/cctv/v1/events/1/snapshot.jpg"),
            "http://192.168.2.21:8095/cctv/v1/events/1/snapshot.jpg"
        );
        assert_eq!(api.url("health"), "http://192.168.2.21:8095/cctv/v1/health");
    }
}
