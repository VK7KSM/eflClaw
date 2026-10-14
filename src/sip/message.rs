//! Just enough SIP message handling for one client account over TLS:
//! parse what the server sends, build what we send. Header names are kept as
//! written; lookups are case-insensitive and understand the compact forms.

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Largest message accepted from the server (headers + body).
const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartLine {
    Request { method: String, uri: String },
    Response { code: u16, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipMessage {
    pub start: StartLine,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn full_name(name: &str) -> &str {
    match name.to_ascii_lowercase().as_str() {
        "v" => "via",
        "f" => "from",
        "t" => "to",
        "i" => "call-id",
        "m" => "contact",
        "l" => "content-length",
        "c" => "content-type",
        "k" => "supported",
        "e" => "content-encoding",
        _ => name,
    }
}

fn same_header(a: &str, b: &str) -> bool {
    full_name(a).eq_ignore_ascii_case(full_name(b))
}

impl SipMessage {
    pub fn request(method: &str, uri: &str) -> Self {
        Self {
            start: StartLine::Request {
                method: method.to_string(),
                uri: uri.to_string(),
            },
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn response(code: u16, reason: &str) -> Self {
        Self {
            start: StartLine::Response {
                code,
                reason: reason.to_string(),
            },
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.body = body;
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| same_header(n, name))
            .map(|(_, v)| v.as_str())
    }

    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(n, _)| same_header(n, name))
            .flat_map(|(_, v)| split_list(v))
    }

    pub fn code(&self) -> Option<u16> {
        match &self.start {
            StartLine::Response { code, .. } => Some(*code),
            StartLine::Request { .. } => None,
        }
    }

    pub fn method(&self) -> Option<&str> {
        match &self.start {
            StartLine::Request { method, .. } => Some(method),
            StartLine::Response { .. } => None,
        }
    }

    pub fn reason(&self) -> &str {
        match &self.start {
            StartLine::Response { reason, .. } => reason,
            StartLine::Request { .. } => "",
        }
    }

    pub fn call_id(&self) -> Option<&str> {
        self.get("call-id")
    }

    /// `branch` parameter of the top Via.
    pub fn branch(&self) -> Option<&str> {
        self.get_all("via").next().and_then(|v| param(v, "branch"))
    }

    /// (number, method) of CSeq.
    pub fn cseq(&self) -> Option<(u32, &str)> {
        let v = self.get("cseq")?;
        let (n, m) = v.trim().split_once(char::is_whitespace)?;
        Some((n.parse().ok()?, m.trim()))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = match &self.start {
            StartLine::Request { method, uri } => format!("{method} {uri} SIP/2.0\r\n"),
            StartLine::Response { code, reason } => format!("SIP/2.0 {code} {reason}\r\n"),
        };
        for (n, v) in &self.headers {
            if same_header(n, "content-length") {
                continue;
            }
            out.push_str(n);
            out.push_str(": ");
            out.push_str(v);
            out.push_str("\r\n");
        }
        let _ = write!(out, "Content-Length: {}\r\n\r\n", self.body.len());
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }

    pub fn parse(head: &str, body: Vec<u8>) -> Result<Self> {
        let mut lines = head.split("\r\n").filter(|l| !l.is_empty());
        let first = lines.next().context("empty SIP message")?;
        let start = if let Some(rest) = first.strip_prefix("SIP/2.0 ") {
            let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
            StartLine::Response {
                code: code.trim().parse().context("bad status code")?,
                reason: reason.trim().to_string(),
            }
        } else {
            let mut parts = first.split_whitespace();
            let method = parts.next().context("no method")?;
            let uri = parts.next().context("no request URI")?;
            if parts.next() != Some("SIP/2.0") {
                bail!("not a SIP request line: {first}");
            }
            StartLine::Request {
                method: method.to_string(),
                uri: uri.to_string(),
            }
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            if line.starts_with([' ', '\t']) {
                // Folded continuation of the previous header.
                if let Some(last) = headers.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(line.trim());
                }
                continue;
            }
            let (n, v) = line.split_once(':').context("header without colon")?;
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
        Ok(Self {
            start,
            headers,
            body,
        })
    }
}

/// Read one message from a stream framed by Content-Length (TLS/TCP).
/// Bare CRLFs between messages (keep-alives) are skipped. `Ok(None)` on EOF.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<SipMessage>> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            if head.is_empty() {
                continue; // keep-alive
            }
            break;
        }
        head.push_str(line.trim_end_matches(['\r', '\n']));
        head.push_str("\r\n");
        if head.len() > MAX_MESSAGE {
            bail!("SIP header block too large");
        }
    }
    let probe = SipMessage::parse(&head, Vec::new())?;
    let len: usize = probe
        .get("content-length")
        .map(|v| v.trim().parse())
        .transpose()
        .context("bad Content-Length")?
        .unwrap_or(0);
    if len > MAX_MESSAGE {
        bail!("SIP body too large");
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(SipMessage { body, ..probe }))
}

/// Split a header value on commas that are not inside quotes or angle brackets.
pub fn split_list(v: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth_angle, mut quoted, mut start) = (0i32, false, 0);
    for (i, ch) in v.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '<' if !quoted => depth_angle += 1,
            '>' if !quoted => depth_angle -= 1,
            ',' if !quoted && depth_angle == 0 => {
                out.push(v[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(v[start..].trim());
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

/// `;name=value` parameter of a header value (outside the `<...>` part).
pub fn param<'a>(v: &'a str, name: &str) -> Option<&'a str> {
    let after_uri = v.rfind('>').map_or(v, |i| &v[i + 1..]);
    after_uri.split(';').skip(1).find_map(|p| {
        let (k, val) = p.split_once('=').unwrap_or((p, ""));
        k.trim().eq_ignore_ascii_case(name).then(|| val.trim())
    })
}

/// The URI inside `<...>`, or the bare value up to the first `;`.
pub fn uri_of(v: &str) -> &str {
    if let (Some(a), Some(b)) = (v.find('<'), v.find('>')) {
        if a < b {
            return &v[a + 1..b];
        }
    }
    v.split(';').next().unwrap_or(v).trim()
}

/// User part of a SIP URI (`sip:0412345678@host` → `0412345678`).
pub fn uri_user(uri: &str) -> Option<&str> {
    let rest = uri
        .strip_prefix("sip:")
        .or_else(|| uri.strip_prefix("sips:"))
        .or_else(|| uri.strip_prefix("tel:"))?;
    let user = rest.split(['@', ';', '>']).next()?;
    (!user.is_empty()).then_some(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE_OK: &str = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/TLS 192.168.2.71:50000;rport=41000;received=1.2.3.4;branch=z9hG4bKabc\r\n\
Record-Route: <sip:1.2.3.4:5061;transport=tls;lr>\r\n\
From: <sip:108@sip.example>;tag=from1\r\n\
t: <sip:0400000000@sip.example>;tag=as5f\r\n\
Call-ID: call-1\r\n\
CSeq: 2 INVITE\r\n\
Contact: <sip:0400000000@1.2.3.4:5061;transport=tls>\r\n\
Content-Type: application/sdp\r\n\
l: 4\r\n\r\nv=0\n";

    #[tokio::test]
    async fn reads_framed_messages_and_skips_keepalives() {
        let stream = format!("\r\n\r\n{INVITE_OK}SIP/2.0 100 Trying\r\nContent-Length: 0\r\n\r\n");
        let mut r = tokio::io::BufReader::new(stream.as_bytes());
        let m = read_message(&mut r).await.unwrap().unwrap();
        assert_eq!(m.code(), Some(200));
        assert_eq!(m.body, b"v=0\n");
        assert_eq!(m.get("to"), Some("<sip:0400000000@sip.example>;tag=as5f"));
        assert_eq!(param(m.get("to").unwrap(), "tag"), Some("as5f"));
        assert_eq!(m.branch(), Some("z9hG4bKabc"));
        assert_eq!(m.cseq(), Some((2, "INVITE")));
        assert_eq!(
            uri_of(m.get("contact").unwrap()),
            "sip:0400000000@1.2.3.4:5061;transport=tls"
        );
        let m2 = read_message(&mut r).await.unwrap().unwrap();
        assert_eq!(m2.code(), Some(100));
        assert!(read_message(&mut r).await.unwrap().is_none());
    }

    #[test]
    fn builds_requests_with_a_correct_content_length() {
        let m = SipMessage::request("MESSAGE", "sip:0400000000@sip.example")
            .header("Call-ID", "x")
            .with_body("text/plain;charset=UTF-8", "测试".as_bytes().to_vec());
        let text = String::from_utf8(m.to_bytes()).unwrap();
        assert!(text.starts_with("MESSAGE sip:0400000000@sip.example SIP/2.0\r\n"));
        assert!(text.contains("Content-Length: 6\r\n\r\n测试"));
    }

    #[test]
    fn list_splitting_respects_quotes_and_brackets() {
        let v = r#"Digest realm="a,b", nonce="n", <sip:x@y;a=1,2>"#;
        assert_eq!(split_list(v).len(), 3);
        assert_eq!(
            uri_user("sip:0400000000@sip.example;transport=tls"),
            Some("0400000000")
        );
        assert_eq!(uri_user("<sip:108@h>"), None, "callers pass the bare URI");
    }
}
