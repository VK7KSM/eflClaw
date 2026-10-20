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
    /// v1.9: cameras that are not always powered (the roof antenna one);
    /// `Some(false)` means switched off, not a fault.
    #[serde(default)]
    pub powered: Option<bool>,
    /// v1.9: the antenna rotator's state, present on the camera mounted on it.
    #[serde(default)]
    pub rotator: Option<serde_json::Value>,
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
    /// v1.15: whether this event rings the phone, and by which rule. Only
    /// `call` decides; events without it fall back to the old inference.
    #[serde(default)]
    pub call: Option<bool>,
    #[serde(default)]
    pub call_rule: Option<String>,
    /// `kind = "report"` (v1.8): cctv's own summary of a period — no
    /// event id, snapshot or clip; deduplicated by `report_id`.
    #[serde(default)]
    pub report_id: Option<i64>,
    /// `periodic` (every 15 minutes when something happened) or `daily`.
    #[serde(default)]
    pub report_kind: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attention: Vec<Attention>,
}

/// Something a report asks the owner to keep an eye on (v1.8).
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Attention {
    /// `plate` or `note`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub reason: String,
}

/// A person or vehicle the AI tracked in an alarm (v1.5, kept 7 days).
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Object {
    pub idx: u32,
    /// `person` / `vehicle`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub has_face: bool,
    #[serde(default)]
    pub face_px: f64,
    #[serde(default, rename = "match")]
    pub matched: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// The name's category in Chinese (v1.10): 家人 / 邻居 / 朋友 / 陌生人,
    /// 自家车 / 邻居车 / 朋友车 / 陌生车.
    #[serde(default)]
    pub category_zh: Option<String>,
    /// The automatic group this target was put in (v1.11), if any.
    #[serde(default)]
    pub group: Option<Group>,
    /// A vehicle's plate as read (v1.6; `None` when unreadable).
    #[serde(default)]
    pub plate: Option<String>,
    #[serde(default)]
    pub crop_url: String,
}

/// An automatic group (v1.11): cctv's guess that several pictures show the
/// same person or vehicle, under a nickname shown as "〔自动〕…". Not a name.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Group {
    pub id: i64,
    #[serde(default)]
    pub display: String,
}

/// Who did it, on every request a person triggers (cctv's unified fields):
/// `by` is always "telegram" here; `account` is the person (1–64 chars).
fn who(mut body: serde_json::Value, account: &str) -> serde_json::Value {
    body["by"] = "telegram".into();
    body["account"] = account.into();
    body
}

/// The `account` for a person: their Telegram username, or `fallback` (the
/// chat id) when they have none; never empty, at most 64 characters.
pub fn account_of(user: &str, fallback: &str) -> String {
    let who = if user.trim().is_empty() || user == "unknown" {
        fallback
    } else {
        user
    };
    let who = if who.trim().is_empty() {
        "elfclaw"
    } else {
        who
    };
    who.chars().take(64).collect()
}

/// A delivery report for `POST /events/{id}/delivered` (v1.13). `note`
/// (why it failed) is cut to 200 characters as the contract asks.
pub fn delivery(channel: &str, accounts: Option<usize>, ok: bool, note: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "channel": channel,
        "ok": ok,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    match channel {
        "call" => v["call"] = true.into(),
        "sms" => v["sms"] = true.into(),
        _ => {}
    }
    if let Some(n) = accounts {
        v["accounts"] = n.into();
    }
    if !note.is_empty() {
        v["note"] = note.chars().take(200).collect::<String>().into();
    }
    v
}

/// A registered person or vehicle (v1.10: with its category).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Name {
    pub name: String,
    pub category_zh: Option<String>,
}

/// Allowed categories (v1.10). Only family / own stop the alarms.
pub const PERSON_CATEGORIES: [&str; 4] = ["family", "neighbor", "friend", "stranger"];
pub const VEHICLE_CATEGORIES: [&str; 4] = ["own", "neighbor", "friend", "stranger"];

/// What `POST /events/{id}/label` did.
#[derive(Debug, Clone, PartialEq)]
pub enum LabelOutcome {
    /// cctv's note (to be shown verbatim) and the event's (old, new) level.
    Done {
        note: String,
        level: Option<(u8, u8)>,
    },
    /// 422 unlearnable: why (to be shown verbatim).
    Unlearnable(String),
}

/// A label reply, parsed.
fn label_outcome(status: u16, body: &serde_json::Value) -> Option<LabelOutcome> {
    if status == 422 {
        let e = &body["error"];
        let message = e["message"]
            .as_str()
            .or_else(|| body["message"].as_str())
            .unwrap_or("这个目标学不了");
        return Some(LabelOutcome::Unlearnable(message.to_string()));
    }
    if !(200..300).contains(&status) {
        return None;
    }
    let level = |k: &str| body["event"][k].as_u64().and_then(|v| u8::try_from(v).ok());
    Some(LabelOutcome::Done {
        note: body["note"].as_str().unwrap_or("已标注").to_string(),
        level: level("old_level").zip(level("level")),
    })
}

/// One online-AI call cctv-ai made (v1.15.2 `GET /ai/usage`). Gemini calls
/// run on elfClaw's keys and name the key by elfClaw's label in `provider`
/// (`gemini`, `gemini#2` … `gemini#6`), never the key itself.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UsageRecord {
    pub id: i64,
    pub t_ms: i64,
    pub provider: String,
    pub model: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsagePage {
    pub records: Vec<UsageRecord>,
    pub next_after_id: i64,
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

    async fn patch_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let resp = self
            .http
            .patch(self.url(path))
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
    pub async fn clip_live(&self, monitor: u32, seconds: u32, account: &str) -> Result<String> {
        let v: serde_json::Value = self
            .post_json(
                &format!("monitors/{monitor}/clip"),
                &who(serde_json::json!({"seconds": seconds}), account),
            )
            .await?;
        v["job_id"]
            .as_str()
            .map(str::to_string)
            .context("no job id")
    }

    /// Recorded video from the NVR (≤ 5 min, ending ≥ 2 min ago); job id.
    pub async fn clip_history(
        &self,
        monitor: u32,
        from: &str,
        to: &str,
        account: &str,
    ) -> Result<String> {
        let v: serde_json::Value = self
            .post_json(
                "clips",
                &who(
                    serde_json::json!({"monitor_id": monitor, "from": from, "to": to}),
                    account,
                ),
            )
            .await?;
        v["job_id"]
            .as_str()
            .map(str::to_string)
            .context("no job id")
    }

    pub async fn ptz(&self, body: &serde_json::Value, account: &str) -> Result<()> {
        let _: serde_json::Value = self.post_json("ptz", &who(body.clone(), account)).await?;
        Ok(())
    }

    /// The antenna rotator's state (v1.9): az, el, moving, controller, locked…
    pub async fn rotator(&self, monitor: u32) -> Result<serde_json::Value> {
        self.get_json(
            &format!("ptz?monitor_id={monitor}"),
            Duration::from_secs(10),
        )
        .await
    }

    pub async fn arm_status(&self) -> Result<serde_json::Value> {
        self.get_json("arm", Duration::from_secs(10)).await
    }

    pub async fn set_arm(
        &self,
        body: &serde_json::Value,
        account: &str,
    ) -> Result<serde_json::Value> {
        self.post_json("arm", &who(body.clone(), account)).await
    }

    /// The people and vehicles the AI tracked in an alarm (empty for alarms
    /// before the AI rework or older than 7 days).
    pub async fn objects(&self, event_id: i64) -> Result<Vec<Object>> {
        #[derive(Deserialize)]
        struct O {
            #[serde(default)]
            objects: Vec<Object>,
        }
        Ok(self
            .get_json::<O>(
                &format!("events/{event_id}/objects"),
                Duration::from_secs(15),
            )
            .await?
            .objects)
    }

    async fn names(&self, path: &str, field: &str) -> Result<Vec<Name>> {
        let v: serde_json::Value = self.get_json(path, Duration::from_secs(10)).await?;
        Ok(v[field]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        Some(Name {
                            name: p["name"].as_str()?.to_string(),
                            category_zh: p["category_zh"].as_str().map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Registered people.
    pub async fn people(&self) -> Result<Vec<Name>> {
        self.names("people", "people").await
    }

    /// Registered vehicles.
    pub async fn vehicles(&self) -> Result<Vec<Name>> {
        self.names("vehicles", "vehicles").await
    }

    /// The automatic groups not handled yet (v1.11): `auto_people` and
    /// `auto_vehicles` of `/people` and `/vehicles`.
    pub async fn auto_groups(&self) -> Result<(serde_json::Value, serde_json::Value)> {
        let p: serde_json::Value = self.get_json("people", Duration::from_secs(10)).await?;
        let v: serde_json::Value = self.get_json("vehicles", Duration::from_secs(10)).await?;
        Ok((p["auto_people"].clone(), v["auto_vehicles"].clone()))
    }

    /// Label a whole automatic group as `name` (v1.11); cctv's note.
    pub async fn group_label(
        &self,
        id: i64,
        name: &str,
        category: Option<&str>,
        account: &str,
    ) -> Result<String> {
        let mut body = who(serde_json::json!({"name": name}), account);
        if let Some(c) = category {
            body["category"] = c.into();
        }
        let v: serde_json::Value = self.post_json(&format!("groups/{id}/label"), &body).await?;
        Ok(v["note"].as_str().unwrap_or("已标注").to_string())
    }

    /// Ignore a whole automatic group, now and later (v1.11); cctv's note.
    pub async fn group_ignore(&self, id: i64, account: &str) -> Result<String> {
        let v: serde_json::Value = self
            .post_json(
                &format!("groups/{id}/ignore"),
                &who(serde_json::json!({}), account),
            )
            .await?;
        Ok(v["note"].as_str().unwrap_or("已忽略").to_string())
    }

    /// Change a registered name's category (v1.10); cctv's note, to be
    /// shown verbatim. `kind` is `person` or `vehicle`.
    pub async fn set_category(
        &self,
        kind: &str,
        name: &str,
        category: &str,
        account: &str,
    ) -> Result<String> {
        let v: serde_json::Value = self
            .post_json(
                "names/category",
                &who(
                    serde_json::json!({"kind": kind, "name": name, "category": category}),
                    account,
                ),
            )
            .await?;
        Ok(v["note"].as_str().unwrap_or("已设置").to_string())
    }

    /// Label target `object` (an index, or `"all"`) of an alarm as `name`.
    pub async fn label(
        &self,
        event_id: i64,
        object: &serde_json::Value,
        name: &str,
        category: Option<&str>,
        account: &str,
    ) -> Result<LabelOutcome> {
        let mut body = who(serde_json::json!({"object": object, "name": name}), account);
        if let Some(c) = category {
            body["category"] = c.into();
        }
        let resp = self
            .http
            .post(self.url(&format!("events/{event_id}/label")))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .json(&body)
            .send()
            .await
            .context("cctv-server unreachable")?;
        let status = resp.status().as_u16();
        if status != 422 && !resp.status().is_success() {
            Self::check(resp).await?;
            bail!("cctv-server {status}");
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        label_outcome(status, &body).context("bad cctv-server reply")
    }

    /// Undo the label of one target.
    pub async fn unlabel(&self, event_id: i64, object: u32, account: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                &format!("events/{event_id}/unlabel"),
                &who(serde_json::json!({"object": object}), account),
            )
            .await?;
        Ok(())
    }

    /// Who is at each camera right now (v1.7, queries only).
    pub async fn scene(&self) -> Result<serde_json::Value> {
        self.get_json("scene", Duration::from_secs(10)).await
    }

    /// Recent summaries and the watch list (v1.8).
    pub async fn reports(&self, limit: u32) -> Result<serde_json::Value> {
        self.get_json(&format!("reports?limit={limit}"), Duration::from_secs(15))
            .await
    }

    /// Tell cctv the owner said "我在家" (v1.14): recorded and shown there
    /// only — cctv's judgement and levels do not change.
    pub async fn presence(&self, minutes: i64, account: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                "presence",
                &serde_json::json!({"home": true, "minutes": minutes, "by": "telegram", "account": account}),
            )
            .await?;
        Ok(())
    }

    /// Feedback on a call (v1.15 §16). `reason` only when not useful.
    /// Returns cctv's `ask_label` (family or friends: label them next).
    pub async fn call_feedback(
        &self,
        event_id: i64,
        useful: bool,
        reason: Option<&str>,
        account: &str,
    ) -> Result<bool> {
        let mut body = who(serde_json::json!({"useful": useful}), account);
        if let Some(r) = reason.filter(|_| !useful) {
            body["reason"] = r.into();
        }
        let v: serde_json::Value = self
            .post_json(&format!("events/{event_id}/call_feedback"), &body)
            .await?;
        Ok(v["ask_label"].as_bool().unwrap_or(false))
    }

    /// Feedback on a "cctv unreachable" call, which has no event.
    pub async fn call_feedback_unreachable(
        &self,
        at: &str,
        useful: bool,
        reason: Option<&str>,
        account: &str,
    ) -> Result<()> {
        let mut body = who(
            serde_json::json!({"kind": "cctv_unreachable", "at": at, "useful": useful}),
            account,
        );
        if let Some(r) = reason.filter(|_| !useful) {
            body["reason"] = r.into();
        }
        let _: serde_json::Value = self.post_json("call_feedback", &body).await?;
        Ok(())
    }

    /// Who and what one camera sees now (v1.15.1 §20): the people and cars
    /// of cctv's live scene for that camera, with a one-line `summary`.
    pub async fn now(&self, monitor: u32) -> Result<serde_json::Value> {
        self.get_json(&format!("monitors/{monitor}/now"), Duration::from_secs(10))
            .await
    }

    /// cctv's one-line answer to "what is at this camera now", or why not.
    pub async fn now_summary(&self, monitor: u32) -> String {
        match self.now(monitor).await {
            Ok(v) => v["summary"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| "（cctv 没有给出现场情况）".into()),
            Err(e) => format!("（取不到现场情况：{e:#}）"),
        }
    }

    /// Everything the panel's "📊 状态" shows (v1.14 §13).
    pub async fn status(&self) -> Result<serde_json::Value> {
        self.get_json("status", Duration::from_secs(15)).await
    }

    /// cctv-ai's online-AI calls after `after_id`, oldest first (v1.15.2).
    pub async fn ai_usage(&self, after_id: i64, limit: usize) -> Result<UsagePage> {
        self.get_json(
            &format!("ai/usage?after_id={after_id}&limit={limit}"),
            Duration::from_secs(15),
        )
        .await
    }

    /// K5's whole-machine load (v1.15.3 §22): the latest reading plus one
    /// summary per hour over the last `hours` (1–720), no per-minute rows.
    pub async fn sysload(&self, hours: u32) -> Result<serde_json::Value> {
        self.get_json(
            &format!("sysload?hours={}&minutes=0", hours.clamp(1, 720)),
            Duration::from_secs(15),
        )
        .await
    }

    /// cctv's run log (v1.14 §14): what it did and why. `query` is the raw
    /// query string (event_id, monitor, kind, from, to, since_rev, limit).
    pub async fn runlog(&self, query: &str) -> Result<serde_json::Value> {
        self.get_json(&format!("runlog?{query}"), Duration::from_secs(15))
            .await
    }

    /// The guarding memory (v1.15 §17): `query` is q, kind, active, limit.
    pub async fn memory_list(&self, query: &str) -> Result<serde_json::Value> {
        self.get_json(&format!("memory?{query}"), Duration::from_secs(15))
            .await
    }

    /// Something the user said, written as they said it (`source = user`).
    pub async fn memory_add(
        &self,
        kind: &str,
        content: &str,
        quote: &str,
        days: Option<u64>,
        account: &str,
    ) -> Result<serde_json::Value> {
        let mut body = who(
            serde_json::json!({"kind": kind, "content": content, "source": "user", "quote": quote}),
            account,
        );
        if let Some(d) = days {
            body["days"] = d.into();
        }
        self.post_json("memory", &body).await
    }

    /// Change a memory as the user (`changes`: kind, content, quote, days).
    pub async fn memory_update(
        &self,
        id: i64,
        changes: serde_json::Value,
        account: &str,
    ) -> Result<serde_json::Value> {
        let mut body = who(changes, account);
        body["source"] = "user".into();
        self.patch_json(&format!("memory/{id}"), &body).await
    }

    pub async fn memory_revoke(
        &self,
        id: i64,
        reason: Option<&str>,
        account: &str,
    ) -> Result<serde_json::Value> {
        let mut body = who(serde_json::json!({"source": "user"}), account);
        if let Some(r) = reason {
            body["reason"] = r.into();
        }
        self.post_json(&format!("memory/{id}/revoke"), &body).await
    }

    /// "我出门": nobody home from now on (v1.14).
    pub async fn presence_away(&self, account: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                "presence",
                &serde_json::json!({"home": false, "by": "telegram", "account": account}),
            )
            .await?;
        Ok(())
    }

    /// Greet at a camera (v1.4): `hello` plays a chime and a spoken English
    /// greeting on its speaker, `flash` blinks HI in Morse on its light.
    pub async fn greet(&self, monitor: u32, mode: &str, account: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                "greet",
                &who(
                    serde_json::json!({"monitor_id": monitor, "mode": mode}),
                    account,
                ),
            )
            .await?;
        Ok(())
    }

    /// Tell cctv-server when the Telegram push went out (v1.4, for its
    /// end-to-end latency statistics).
    /// `report` is the v1.13 body: channel (telegram / call / sms),
    /// accounts, call, sms, ok, note, at — see [`delivery`].
    pub async fn delivered(&self, event_id: i64, report: &serde_json::Value) -> Result<()> {
        let resp = self
            .http
            .post(self.url(&format!("events/{event_id}/delivered")))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .json(report)
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
    fn person_triggered_requests_say_who() {
        let b = who(serde_json::json!({"seconds": 15}), "zeroclaw_user");
        assert_eq!(b["by"], "telegram");
        assert_eq!(b["account"], "zeroclaw_user");
        assert_eq!(b["seconds"], 15);
        assert_eq!(account_of("unknown", "100"), "100");
        assert_eq!(account_of("", ""), "elfclaw");
        assert_eq!(account_of(&"x".repeat(80), "100").chars().count(), 64);
    }

    #[test]
    fn delivery_reports_follow_the_contract() {
        let t = delivery("telegram", Some(2), true, "");
        assert_eq!(
            (
                t["channel"].as_str(),
                t["accounts"].as_u64(),
                t["ok"].as_bool()
            ),
            (Some("telegram"), Some(2), Some(true))
        );
        assert!(t.get("note").is_none() && t.get("call").is_none());
        let c = delivery("call", None, false, &"x".repeat(300));
        assert_eq!(c["call"], true);
        assert_eq!(c["ok"], false);
        assert_eq!(c["note"].as_str().unwrap().chars().count(), 200);
        assert_eq!(delivery("sms", None, true, "")["sms"], true);
    }

    #[test]
    fn objects_and_label_replies_parse() {
        let json = r#"{"crop_url":"/cctv/v1/events/36628/objects/0.jpg","face_px":0.0,"faces":0,
            "has_face":false,"idx":0,"kind":"vehicle","label":null,"match":null,"score":null,"status":"parked"}"#;
        let o: Object = serde_json::from_str(json).unwrap();
        assert_eq!(
            (o.idx, o.kind.as_str(), o.status.as_str()),
            (0, "vehicle", "parked")
        );
        assert_eq!(o.matched, None);

        let ok = serde_json::json!({"ok": true, "added": 8, "name": "kasim", "note": "已把1个目标标成「kasim」",
            "event": {"level": 1, "old_level": 3, "description": "认出：kasim"}});
        assert_eq!(
            label_outcome(200, &ok),
            Some(LabelOutcome::Done {
                note: "已把1个目标标成「kasim」".into(),
                level: Some((3, 1))
            })
        );
        let no =
            serde_json::json!({"error": {"code": "unlearnable", "message": "脸太小（20 像素）"}});
        assert_eq!(
            label_outcome(422, &no),
            Some(LabelOutcome::Unlearnable("脸太小（20 像素）".into()))
        );
        let flat = serde_json::json!({"error": "unlearnable", "message": "一直没看清脸"});
        assert_eq!(
            label_outcome(422, &flat),
            Some(LabelOutcome::Unlearnable("一直没看清脸".into()))
        );
    }

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
