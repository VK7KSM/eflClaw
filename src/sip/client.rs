//! One SIP account on one TLS connection: register and keep registered, send
//! and receive SMS (SIP MESSAGE through the server's GSM gateway), and place
//! "press 1 to confirm" alarm calls.
//!
//! Deliberately small. TLS is a reliable transport, so none of RFC 3261's
//! retransmission timers apply; there is one account, outgoing calls only
//! (incoming INVITEs are refused), and one call at a time — the gateway can
//! only carry one anyway.

use super::digest::{self, Challenge};
use super::media::{self, MediaEnd, PlayOptions};
use super::message::{self, read_message, SipMessage};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex as SyncMutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader, WriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, watch, Mutex, RwLock};
use tokio_rustls::client::TlsStream;

const USER_AGENT: &str = "elfClaw";
const REGISTER_EXPIRES: u32 = 300;
const KEEPALIVE: Duration = Duration::from_secs(30);
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(32);
/// How long an unanswered INVITE may ring before it is cancelled.
const INVITE_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Account details. `Debug` never shows the password.
#[derive(Clone)]
pub struct SipAccount {
    pub server: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for SipAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SipAccount")
            .field("server", &self.server)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"***")
            .finish()
    }
}

impl SipAccount {
    /// Read `Account <user>` / `Password <password>` lines (the file the
    /// owner keeps next to the instance; labels are case-insensitive and may
    /// be followed by `:` or `=`).
    pub fn from_credentials(text: &str, server: &str, port: u16) -> Result<Self> {
        let mut user = None;
        let mut password = None;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (label, value) = line
                .split_once(|c: char| c.is_whitespace() || c == ':' || c == '=')
                .context("credential line without a value")?;
            let value = value.trim().trim_start_matches([':', '=']).trim();
            match label.to_ascii_lowercase().as_str() {
                "account" | "user" | "username" | "extension" => user = Some(value.to_string()),
                "password" | "pass" | "secret" => password = Some(value.to_string()),
                _ => {}
            }
        }
        let user = user.filter(|u| !u.is_empty()).context("no Account line")?;
        let password = password
            .filter(|p| !p.is_empty())
            .context("no Password line")?;
        Ok(Self {
            server: server.to_string(),
            port,
            user,
            password,
        })
    }
}

/// An SMS that arrived for this account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundSms {
    pub from: String,
    pub text: String,
}

/// How the callee showed they got the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// Pressed the digit (works on SIP extensions; VoLTE key presses never
    /// reach the GSM gateway).
    Digit,
    /// Said something the caller's speech check accepted (e.g. "知道了").
    Voice,
    /// Ended the call (answered then hung up, or declined).
    HungUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// Someone responded. Note `HungUp` can also be the network ending a call
    /// it could not complete (phone off, no coverage) — the gateway reports
    /// every end as a plain BYE.
    Responded(Response),
    /// Connected but nobody responded before `max` (unanswered).
    NoResponse,
    /// A voicemail greeting answered; we hung up straight away.
    Voicemail,
    /// The call's audio died (e.g. the gateway crashed); treat as not reached.
    Dropped,
    /// The server or gateway refused the call (e.g. 486: the line is in use).
    NotConnected { code: u16, reason: String },
}

pub struct CallOptions {
    pub prompt: Arc<Vec<i16>>,
    pub confirm: Option<Arc<Vec<i16>>>,
    pub gap: Duration,
    pub max: Duration,
    pub digit: char,
    /// See `media::PlayOptions::speech_check`.
    pub speech_check: Option<media::SpeechCheck>,
}

/// Numbers the server routes: internal extensions (3–4 digits) and, through
/// the gateway, 04xxxxxxxx, +…, 00…, 1 + 10 digits.
pub fn is_dialable(number: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    ((3..=4).contains(&number.len()) && !number.starts_with('0') && digits(number))
        || (number.len() == 10 && number.starts_with("04") && digits(number))
        || number
            .strip_prefix('+')
            .is_some_and(|r| r.len() >= 6 && digits(r))
        || (number.starts_with("00") && number.len() >= 6 && digits(number))
        || (number.len() == 11 && number.starts_with('1') && digits(number))
}

/// Mask a phone number for logs: keep the last three digits.
pub fn masked(number: &str) -> String {
    let n = number.chars().count();
    number
        .chars()
        .enumerate()
        .map(|(i, c)| if i + 3 < n { '*' } else { c })
        .collect()
}

struct Conn {
    writer: Mutex<WriteHalf<TlsStream<TcpStream>>>,
    local: SocketAddr,
    /// Our client transactions, by Via branch.
    waiters: SyncMutex<HashMap<String, mpsc::UnboundedSender<SipMessage>>>,
    /// Requests the server sends inside one of our calls, by Call-ID.
    dialogs: SyncMutex<HashMap<String, mpsc::UnboundedSender<SipMessage>>>,
}

impl Conn {
    async fn send(&self, m: &SipMessage) -> Result<()> {
        let mut w = self.writer.lock().await;
        w.write_all(&m.to_bytes()).await?;
        w.flush().await?;
        Ok(())
    }
}

struct Inner {
    account: SipAccount,
    conn: RwLock<Option<Arc<Conn>>>,
    registered: watch::Sender<bool>,
    sms: broadcast::Sender<InboundSms>,
    call_lock: Mutex<()>,
    reg_call_id: String,
    reg_cseq: AtomicU32,
}

/// Handle to the running client; cheap to clone.
#[derive(Clone)]
pub struct SipClient {
    inner: Arc<Inner>,
}

fn token() -> String {
    format!("{:016x}", rand::random::<u64>())
}

impl SipClient {
    /// Start connecting and registering in the background. The client keeps
    /// reconnecting (5 s doubling to 5 min) for as long as the process runs.
    pub fn start(account: SipAccount) -> Self {
        let (registered, _) = watch::channel(false);
        let (sms, _) = broadcast::channel(32);
        let inner = Arc::new(Inner {
            reg_call_id: format!("{}@elfclaw", token()),
            account,
            conn: RwLock::new(None),
            registered,
            sms,
            call_lock: Mutex::new(()),
            reg_cseq: AtomicU32::new(0),
        });
        tokio::spawn(supervise(inner.clone()));
        Self { inner }
    }

    pub fn is_registered(&self) -> bool {
        *self.inner.registered.borrow()
    }

    /// Wait until registered (true) or `timeout` passes (false).
    pub async fn wait_registered(&self, timeout: Duration) -> bool {
        let mut rx = self.inner.registered.subscribe();
        tokio::time::timeout(timeout, rx.wait_for(|r| *r))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    pub fn subscribe_sms(&self) -> broadcast::Receiver<InboundSms> {
        self.inner.sms.subscribe()
    }

    async fn conn(&self) -> Result<Arc<Conn>> {
        self.inner
            .conn
            .read()
            .await
            .clone()
            .filter(|_| self.is_registered())
            .context("SIP 账号未注册（服务器连不上或认证失败）")
    }

    /// Send an SMS. Success means the server accepted it for the gateway,
    /// not that the phone received it.
    pub async fn send_sms(&self, to: &str, text: &str) -> Result<()> {
        if !is_dialable(to) {
            bail!("号码格式不能外发：{}", masked(to));
        }
        let conn = self.conn().await?;
        let a = &self.inner.account;
        let uri = format!("sip:{to}@{}", a.server);
        let call_id = format!("{}@elfclaw", token());
        let from_tag = token();
        let body = text.as_bytes().to_vec();
        let build = |cseq: u32, auth: Option<(&str, String)>| {
            let mut m = base(&conn, a, "MESSAGE", &uri, &call_id, cseq)
                .header(
                    "From",
                    format!("<sip:{}@{}>;tag={from_tag}", a.user, a.server),
                )
                .header("To", format!("<sip:{to}@{}>", a.server));
            if let Some((name, value)) = auth {
                m = m.header(name, value);
            }
            m.with_body("text/plain;charset=UTF-8", body.clone())
        };
        let resp = transact_with_auth(&conn, a, "MESSAGE", &uri, build).await?;
        match resp.code() {
            Some(200..=299) => {
                tracing::info!(to = %masked(to), code = resp.code(), "SIP MESSAGE accepted");
                Ok(())
            }
            code => bail!("短信被服务器拒绝：{} {}", code.unwrap_or(0), resp.reason()),
        }
    }

    /// Call `to`, play the prompt until the digit is pressed, then hang up.
    pub async fn call(&self, to: &str, opts: CallOptions) -> Result<CallOutcome> {
        if !is_dialable(to) {
            bail!("号码格式不能外呼：{}", masked(to));
        }
        let _one_call = self.inner.call_lock.lock().await;
        let conn = self.conn().await?;
        let a = &self.inner.account;
        let socket = UdpSocket::bind(SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), 0))
            .await
            .context("RTP 端口打不开")?;
        let rtp_port = socket.local_addr()?.port();
        let sdp = media::offer(conn.local.ip(), rtp_port, rand::random::<u32>().into());
        let uri = format!("sip:{to}@{}", a.server);
        let call_id = format!("{}@elfclaw", token());
        let from_tag = token();
        let from = format!("<sip:{}@{}>;tag={from_tag}", a.user, a.server);
        let to_hdr = format!("<sip:{to}@{}>", a.server);

        let (dialog_tx, mut dialog_rx) = mpsc::unbounded_channel();
        conn.dialogs.lock().insert(call_id.clone(), dialog_tx);
        let _cleanup = DialogGuard {
            conn: conn.clone(),
            call_id: call_id.clone(),
        };

        tracing::info!(to = %masked(to), "SIP call starting");
        let invite = |cseq: u32, auth: Option<(&str, String)>| {
            let mut m = base(&conn, a, "INVITE", &uri, &call_id, cseq)
                .header("From", from.clone())
                .header("To", to_hdr.clone());
            if let Some((name, value)) = auth {
                m = m.header(name, value);
            }
            m.with_body("application/sdp", sdp.clone().into_bytes())
        };

        let mut cseq = 1;
        let mut request = invite(cseq, None);
        let mut authed = false;
        let ok = loop {
            let resp = invite_once(&conn, &request).await?;
            let code = resp.code().unwrap_or(0);
            if (200..300).contains(&code) {
                break resp;
            }
            // Every non-2xx final answer to an INVITE is acknowledged in the
            // same transaction.
            conn.send(&ack_for_failure(&request, &resp)).await?;
            if (code == 401 || code == 407) && !authed {
                let (name, value) = auth_header(a, &resp, "INVITE", &uri)?;
                cseq += 1;
                request = invite(cseq, Some((name, value)));
                authed = true;
                continue;
            }
            tracing::warn!(to = %masked(to), code, reason = resp.reason(), "SIP call not connected");
            return Ok(CallOutcome::NotConnected {
                code,
                reason: resp.reason().to_string(),
            });
        };

        // Dialog established.
        let to_tagged = ok.get("to").unwrap_or(&to_hdr).to_string();
        let target = ok
            .get("contact")
            .map(|c| message::uri_of(c).to_string())
            .unwrap_or_else(|| uri.clone());
        let routes: Vec<String> = ok
            .get_all("record-route")
            .map(str::to_string)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let in_dialog = |method: &str, cseq: u32| {
            let mut m = base(&conn, a, method, &target, &call_id, cseq)
                .header("From", from.clone())
                .header("To", to_tagged.clone());
            for r in &routes {
                m = m.header("Route", r.clone());
            }
            m
        };
        conn.send(&in_dialog("ACK", cseq)).await?;
        let answer = media::parse_answer(&String::from_utf8_lossy(&ok.body));

        let (hangup_tx, hangup_rx) = watch::channel(false);
        let reply_conn = conn.clone();
        let watcher = tokio::spawn(async move {
            while let Some(req) = dialog_rx.recv().await {
                let method = req.method().unwrap_or("").to_ascii_uppercase();
                if method == "ACK" {
                    continue;
                }
                let _ = reply_conn.send(&response_to(&req, 200, "OK")).await;
                if method == "BYE" {
                    let _ = hangup_tx.send(true);
                    break;
                }
            }
        });

        let end = match answer {
            Ok(answer) => {
                let play = PlayOptions {
                    prompt: opts.prompt,
                    gap: opts.gap,
                    max: opts.max,
                    digit: opts.digit,
                    speech_check: opts.speech_check,
                    confirm: opts.confirm,
                };
                media::play_and_listen(&socket, &answer, &play, hangup_rx).await?
            }
            Err(e) => {
                tracing::warn!("SIP call answer unusable: {e:#}");
                MediaEnd::TimedOut
            }
        };
        watcher.abort();
        if !matches!(end, MediaEnd::RemoteHangup | MediaEnd::MediaLost) {
            // Hang up; a failure here changes nothing for the caller.
            let bye = |cseq: u32, auth: Option<(&str, String)>| {
                let mut m = in_dialog("BYE", cseq);
                if let Some((name, value)) = auth {
                    m = m.header(name, value);
                }
                m
            };
            let first = cseq + 1;
            let _ = transact_with_auth_from(&conn, a, "BYE", &target, first, bye).await;
        }
        let outcome = match end {
            MediaEnd::Digit => CallOutcome::Responded(Response::Digit),
            MediaEnd::Voice => CallOutcome::Responded(Response::Voice),
            MediaEnd::RemoteHangup => CallOutcome::Responded(Response::HungUp),
            MediaEnd::TimedOut => CallOutcome::NoResponse,
            MediaEnd::MediaLost => CallOutcome::Dropped,
            MediaEnd::Voicemail => CallOutcome::Voicemail,
        };
        tracing::info!(to = %masked(to), ?outcome, "SIP call finished");
        Ok(outcome)
    }
}

struct DialogGuard {
    conn: Arc<Conn>,
    call_id: String,
}

impl Drop for DialogGuard {
    fn drop(&mut self) {
        self.conn.dialogs.lock().remove(&self.call_id);
    }
}

/// Request skeleton: Via (new branch), Max-Forwards, Call-ID, CSeq, Contact, UA.
fn base(
    conn: &Conn,
    a: &SipAccount,
    method: &str,
    uri: &str,
    call_id: &str,
    cseq: u32,
) -> SipMessage {
    let local = conn.local;
    SipMessage::request(method, uri)
        .header(
            "Via",
            format!("SIP/2.0/TLS {local};rport;branch=z9hG4bK{}", token()),
        )
        .header("Max-Forwards", "70")
        .header("Call-ID", call_id)
        .header("CSeq", format!("{cseq} {method}"))
        .header(
            "Contact",
            format!("<sip:{}@{local};transport=tls;ob>", a.user),
        )
        .header("User-Agent", USER_AGENT)
}

/// The Authorization / Proxy-Authorization header answering `resp`'s challenge.
fn auth_header(
    a: &SipAccount,
    resp: &SipMessage,
    method: &str,
    uri: &str,
) -> Result<(&'static str, String)> {
    let (challenge_name, header_name) = if resp.code() == Some(407) {
        ("proxy-authenticate", "Proxy-Authorization")
    } else {
        ("www-authenticate", "Authorization")
    };
    let challenge: Challenge = resp
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(challenge_name))
        .find_map(|(_, v)| digest::parse_challenge(v).ok())
        .context("服务器要求认证，但没有可用的 MD5 摘要挑战")?;
    let value = digest::authorization(&challenge, &a.user, &a.password, method, uri, 1, &token());
    Ok((header_name, value))
}

/// Send `msg` and wait for its final response (1xx ignored).
async fn transact(conn: &Conn, msg: &SipMessage) -> Result<SipMessage> {
    let branch = msg.branch().context("request without branch")?.to_string();
    let (tx, mut rx) = mpsc::unbounded_channel();
    conn.waiters.lock().insert(branch.clone(), tx);
    let result = async {
        conn.send(msg).await?;
        let deadline = Instant::now() + TRANSACTION_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let resp = tokio::time::timeout(left, rx.recv())
                .await
                .context("SIP 服务器没有回应")?
                .context("SIP 连接已断开")?;
            if resp.code().unwrap_or(0) >= 200 {
                return Ok(resp);
            }
        }
    }
    .await;
    conn.waiters.lock().remove(&branch);
    result
}

/// `transact`, answering one digest challenge. `build(cseq, auth)` makes the
/// request; the challenged retry uses the next CSeq and a fresh branch.
async fn transact_with_auth<F>(
    conn: &Conn,
    a: &SipAccount,
    method: &str,
    uri: &str,
    build: F,
) -> Result<SipMessage>
where
    F: Fn(u32, Option<(&str, String)>) -> SipMessage,
{
    transact_with_auth_from(conn, a, method, uri, 1, build).await
}

async fn transact_with_auth_from<F>(
    conn: &Conn,
    a: &SipAccount,
    method: &str,
    uri: &str,
    first_cseq: u32,
    build: F,
) -> Result<SipMessage>
where
    F: Fn(u32, Option<(&str, String)>) -> SipMessage,
{
    let resp = transact(conn, &build(first_cseq, None)).await?;
    match resp.code() {
        Some(401 | 407) => {
            let (name, value) = auth_header(a, &resp, method, uri)?;
            transact(conn, &build(first_cseq + 1, Some((name, value)))).await
        }
        _ => Ok(resp),
    }
}

/// Send an INVITE and wait for its final response, cancelling it if it only
/// rings for longer than `INVITE_TIMEOUT`.
async fn invite_once(conn: &Conn, invite: &SipMessage) -> Result<SipMessage> {
    let branch = invite
        .branch()
        .context("INVITE without branch")?
        .to_string();
    let (tx, mut rx) = mpsc::unbounded_channel();
    conn.waiters.lock().insert(branch.clone(), tx);
    let result = async {
        conn.send(invite).await?;
        let deadline = Instant::now() + INVITE_TIMEOUT;
        let mut cancelled = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(resp)) => {
                    let code = resp.code().unwrap_or(0);
                    if code >= 200
                        && !(cancelled && resp.cseq().is_some_and(|(_, m)| m == "CANCEL"))
                    {
                        return Ok(resp);
                    }
                }
                Ok(None) => bail!("SIP 连接已断开"),
                Err(_) if !cancelled => {
                    conn.send(&cancel_for(invite)).await?;
                    cancelled = true;
                }
                Err(_) => bail!("呼叫取消后服务器没有回应"),
            }
        }
    }
    .await;
    conn.waiters.lock().remove(&branch);
    result
}

/// CANCEL for a pending INVITE: same Request-URI, Via, From, To, Call-ID.
fn cancel_for(invite: &SipMessage) -> SipMessage {
    let uri = match &invite.start {
        message::StartLine::Request { uri, .. } => uri.clone(),
        message::StartLine::Response { .. } => String::new(),
    };
    let mut m = SipMessage::request("CANCEL", &uri);
    for name in ["Via", "Max-Forwards", "From", "To", "Call-ID", "Route"] {
        for v in invite.get_all(name) {
            m = m.header(name, v);
        }
    }
    let n = invite.cseq().map_or(1, |(n, _)| n);
    m.header("CSeq", format!("{n} CANCEL"))
        .header("User-Agent", USER_AGENT)
}

/// ACK for a non-2xx final response: same branch, the response's To (tag).
fn ack_for_failure(invite: &SipMessage, resp: &SipMessage) -> SipMessage {
    let uri = match &invite.start {
        message::StartLine::Request { uri, .. } => uri.clone(),
        message::StartLine::Response { .. } => String::new(),
    };
    let mut m = SipMessage::request("ACK", &uri);
    if let Some(v) = invite.get_all("via").next() {
        m = m.header("Via", v);
    }
    for name in ["Max-Forwards", "From", "Call-ID", "Route"] {
        for v in invite.get_all(name) {
            m = m.header(name, v);
        }
    }
    let n = invite.cseq().map_or(1, |(n, _)| n);
    m.header("To", resp.get("to").or(invite.get("to")).unwrap_or(""))
        .header("CSeq", format!("{n} ACK"))
        .header("User-Agent", USER_AGENT)
}

/// A response to a request the server sent us.
fn response_to(req: &SipMessage, code: u16, reason: &str) -> SipMessage {
    let mut m = SipMessage::response(code, reason);
    for (n, v) in &req.headers {
        if n.eq_ignore_ascii_case("via") || n.eq_ignore_ascii_case("v") {
            m = m.header("Via", v.clone());
        }
    }
    let to = req.get("to").unwrap_or("").to_string();
    let to = if message::param(&to, "tag").is_some() {
        to
    } else {
        format!("{to};tag={}", token())
    };
    m.header("From", req.get("from").unwrap_or(""))
        .header("To", to)
        .header("Call-ID", req.call_id().unwrap_or(""))
        .header("CSeq", req.get("cseq").unwrap_or(""))
        .header("User-Agent", USER_AGENT)
}

// ── connection lifecycle ────────────────────────────────────────────────────

async fn supervise(inner: Arc<Inner>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        match run_connection(&inner).await {
            Ok(()) => tracing::warn!("SIP connection closed by the server"),
            Err(e) => tracing::warn!("SIP connection failed: {e:#}"),
        }
        *inner.conn.write().await = None;
        inner.registered.send_replace(false);
        if started.elapsed() > Duration::from_secs(300) {
            backoff = BACKOFF_MIN;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn run_connection(inner: &Arc<Inner>) -> Result<()> {
    let a = &inner.account;
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((a.server.as_str(), a.port)),
    )
    .await
    .context("连接 SIP 服务器超时")??;
    let local = tcp.local_addr()?;
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = rustls::pki_types::ServerName::try_from(a.server.clone())?;
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, tcp))
        .await
        .context("SIP TLS 握手超时")??;
    let (reader, writer) = tokio::io::split(tls);
    let conn = Arc::new(Conn {
        writer: Mutex::new(writer),
        local,
        waiters: SyncMutex::new(HashMap::new()),
        dialogs: SyncMutex::new(HashMap::new()),
    });
    *inner.conn.write().await = Some(conn.clone());

    let mut read_task = tokio::spawn(read_loop(
        BufReader::new(reader),
        conn.clone(),
        inner.clone(),
    ));
    let result = async {
        let mut expires = register(inner, &conn).await?;
        inner.registered.send_replace(true);
        tracing::info!(user = %a.user, server = %a.server, expires, "SIP registered");
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.tick().await;
        let mut refresh_at = Instant::now() + Duration::from_secs(u64::from(expires) * 4 / 5);
        loop {
            tokio::select! {
                done = &mut read_task => {
                    return match done {
                        Ok(r) => r,
                        Err(e) => Err(anyhow::anyhow!("SIP reader stopped: {e}")),
                    };
                }
                _ = keepalive.tick() => {
                    // RFC 5626 CRLF keep-alive: keeps the NAT binding and the
                    // server's connection open.
                    let mut w = conn.writer.lock().await;
                    w.write_all(b"\r\n\r\n").await?;
                    w.flush().await?;
                }
                () = tokio::time::sleep_until(refresh_at.into()) => {
                    expires = register(inner, &conn).await?;
                    refresh_at = Instant::now() + Duration::from_secs(u64::from(expires) * 4 / 5);
                }
            }
        }
    }
    .await;
    read_task.abort();
    result
}

/// REGISTER (with one digest retry); returns the granted expiry in seconds.
async fn register(inner: &Inner, conn: &Conn) -> Result<u32> {
    let a = &inner.account;
    let uri = format!("sip:{}", a.server);
    let aor = format!("<sip:{}@{}>", a.user, a.server);
    let tag = token();
    let build = |cseq: u32, auth: Option<(&str, String)>| {
        let mut m = base(conn, a, "REGISTER", &uri, &inner.reg_call_id, cseq)
            .header("From", format!("{aor};tag={tag}"))
            .header("To", aor.clone())
            .header("Expires", REGISTER_EXPIRES.to_string());
        if let Some((name, value)) = auth {
            m = m.header(name, value);
        }
        m
    };
    let first = inner.reg_cseq.fetch_add(2, Ordering::Relaxed) + 1;
    let resp = transact_with_auth_from(conn, a, "REGISTER", &uri, first, build).await?;
    match resp.code() {
        Some(200..=299) => {
            let granted = resp
                .get_all("contact")
                .find_map(|c| message::param(c, "expires").and_then(|e| e.parse().ok()))
                .or_else(|| resp.get("expires").and_then(|e| e.trim().parse().ok()))
                .unwrap_or(REGISTER_EXPIRES);
            Ok(granted.max(60))
        }
        Some(401 | 403 | 407) => bail!("SIP 注册被拒绝（账号或密码不对）：{}", resp.reason()),
        code => bail!("SIP 注册失败：{} {}", code.unwrap_or(0), resp.reason()),
    }
}

async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    conn: Arc<Conn>,
    inner: Arc<Inner>,
) -> Result<()> {
    while let Some(msg) = read_message(&mut reader).await? {
        if msg.code().is_some() {
            let waiter = msg
                .branch()
                .and_then(|b| conn.waiters.lock().get(b).cloned());
            if let Some(w) = waiter {
                let _ = w.send(msg);
            }
            continue;
        }
        let method = msg.method().unwrap_or("").to_ascii_uppercase();
        let dialog = msg
            .call_id()
            .and_then(|c| conn.dialogs.lock().get(c).cloned());
        if let Some(d) = dialog {
            let _ = d.send(msg);
            continue;
        }
        let reply = match method.as_str() {
            "MESSAGE" => {
                let from = msg
                    .get("from")
                    .and_then(|f| message::uri_user(message::uri_of(f)))
                    .unwrap_or("")
                    .to_string();
                let text = String::from_utf8_lossy(&msg.body).trim().to_string();
                tracing::info!(from = %masked(&from), chars = text.chars().count(), "SMS received");
                let _ = inner.sms.send(InboundSms { from, text });
                Some(response_to(&msg, 200, "OK"))
            }
            "OPTIONS" | "NOTIFY" => Some(response_to(&msg, 200, "OK")),
            "INVITE" => Some(response_to(&msg, 486, "Busy Here")),
            "ACK" => None,
            "BYE" | "CANCEL" | "INFO" => {
                Some(response_to(&msg, 481, "Call/Transaction Does Not Exist"))
            }
            _ => Some(response_to(&msg, 405, "Method Not Allowed")),
        };
        if let Some(r) = reply {
            conn.send(&r).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_file_is_read_and_never_printed() {
        let a = SipAccount::from_credentials(
            "Account 108\r\nPassword s3cr3t@x1\r\n",
            "sip.example",
            5061,
        )
        .unwrap();
        assert_eq!(a.user, "108");
        assert_eq!(a.password, "s3cr3t@x1");
        assert!(!format!("{a:?}").contains("s3cr3t"));
        let b = SipAccount::from_credentials("account: 108\npassword = p w\n", "h", 5061).unwrap();
        assert_eq!(b.password, "p w");
        assert!(SipAccount::from_credentials("Account 108\n", "h", 5061).is_err());
    }

    #[test]
    fn only_numbers_the_server_routes_out_are_dialable() {
        for ok in [
            "0412345678",
            "+61412345678",
            "0061412345678",
            "13800138000",
            "101",
            "1080",
        ] {
            assert!(is_dialable(ok), "{ok}");
        }
        for bad in [
            "041234567",
            "0212345678",
            "012",
            "10",
            "+61 412",
            "sip:x",
            "",
        ] {
            assert!(!is_dialable(bad), "{bad}");
        }
        assert_eq!(masked("0412345678"), "*******678");
    }

    #[test]
    fn replies_mirror_the_request() {
        let req = SipMessage::request("MESSAGE", "sip:108@1.2.3.4")
            .header("Via", "SIP/2.0/TLS 5.6.7.8:5061;branch=z9hG4bKs1")
            .header("Via", "SIP/2.0/TLS 9.9.9.9:5061;branch=z9hG4bKs0")
            .header("From", "<sip:0412345678@sip.example>;tag=f1")
            .header("To", "<sip:108@sip.example>")
            .header("Call-ID", "c1")
            .header("CSeq", "7 MESSAGE");
        let r = response_to(&req, 200, "OK");
        assert_eq!(r.get_all("via").count(), 2);
        assert_eq!(r.branch(), Some("z9hG4bKs1"));
        assert!(message::param(r.get("to").unwrap(), "tag").is_some());
        assert_eq!(r.get("cseq"), Some("7 MESSAGE"));

        let invite = SipMessage::request("INVITE", "sip:0412345678@sip.example")
            .header(
                "Via",
                "SIP/2.0/TLS 192.168.2.71:50000;rport;branch=z9hG4bKi1",
            )
            .header("From", "<sip:108@sip.example>;tag=a")
            .header("To", "<sip:0412345678@sip.example>")
            .header("Call-ID", "c2")
            .header("CSeq", "2 INVITE");
        let busy = SipMessage::response(486, "Busy Here")
            .header("To", "<sip:0412345678@sip.example>;tag=srv");
        let ack = ack_for_failure(&invite, &busy);
        assert_eq!(ack.method(), Some("ACK"));
        assert_eq!(ack.branch(), Some("z9hG4bKi1"), "same transaction");
        assert_eq!(ack.cseq(), Some((2, "ACK")));
        assert_eq!(message::param(ack.get("to").unwrap(), "tag"), Some("srv"));
        let cancel = cancel_for(&invite);
        assert_eq!(cancel.cseq(), Some((2, "CANCEL")));
        assert_eq!(cancel.branch(), Some("z9hG4bKi1"));
    }
}
