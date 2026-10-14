//! HTTP digest authentication for SIP (RFC 3261 §22, RFC 2617), MD5 only —
//! what Asterisk asks for.

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: Option<String>,
    /// `auth` when the server offers it (we never use `auth-int`).
    pub qop_auth: bool,
}

/// Parse a `WWW-Authenticate` / `Proxy-Authenticate` value.
pub fn parse_challenge(v: &str) -> Result<Challenge> {
    let rest = v
        .trim()
        .strip_prefix("Digest")
        .context("only Digest authentication is supported")?;
    let mut realm = None;
    let mut nonce = None;
    let mut opaque = None;
    let mut algorithm = None;
    let mut qop_auth = false;
    for part in crate::sip::message::split_list(rest) {
        let Some((k, val)) = part.split_once('=') else {
            continue;
        };
        let val = val.trim().trim_matches('"').to_string();
        match k.trim().to_ascii_lowercase().as_str() {
            "realm" => realm = Some(val),
            "nonce" => nonce = Some(val),
            "opaque" => opaque = Some(val),
            "algorithm" => algorithm = Some(val),
            "qop" => qop_auth = val.split(',').any(|q| q.trim() == "auth"),
            _ => {}
        }
    }
    if let Some(a) = &algorithm {
        if !a.eq_ignore_ascii_case("MD5") {
            bail!("unsupported digest algorithm {a}");
        }
    }
    Ok(Challenge {
        realm: realm.context("digest challenge without realm")?,
        nonce: nonce.context("digest challenge without nonce")?,
        opaque,
        algorithm,
        qop_auth,
    })
}

fn md5_hex(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    h.finalize()
        .iter()
        .fold(String::with_capacity(32), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// The `Authorization` value for `method uri`. `nc` counts uses of this nonce.
pub fn authorization(
    c: &Challenge,
    user: &str,
    password: &str,
    method: &str,
    uri: &str,
    nc: u32,
    cnonce: &str,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{}:{password}", c.realm));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let nc_hex = format!("{nc:08x}");
    let response = if c.qop_auth {
        md5_hex(&format!("{ha1}:{}:{nc_hex}:{cnonce}:auth:{ha2}", c.nonce))
    } else {
        md5_hex(&format!("{ha1}:{}:{ha2}", c.nonce))
    };
    let mut out = format!(
        "Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\", algorithm=MD5",
        c.realm, c.nonce
    );
    if c.qop_auth {
        let _ = write!(out, ", qop=auth, nc={nc_hex}, cnonce=\"{cnonce}\"");
    }
    if let Some(o) = &c.opaque {
        let _ = write!(out, ", opaque=\"{o}\"");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2617_reference_response() {
        // RFC 2617 §3.5 example (HTTP, but the arithmetic is the same).
        let c = parse_challenge(
            r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
        )
        .unwrap();
        assert!(c.qop_auth);
        let a = authorization(
            &c,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            1,
            "0a4f113b",
        );
        assert!(
            a.contains(r#"response="6629fae49393a05397450978507c4ef1""#),
            "{a}"
        );
        assert!(a.contains("nc=00000001"));
        assert!(a.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
    }

    #[test]
    fn asterisk_style_challenge_without_qop() {
        let c = parse_challenge(
            r#"Digest realm="asterisk",nonce="1697000000/abc",opaque="x",algorithm=md5"#,
        )
        .unwrap();
        assert!(!c.qop_auth);
        let a = authorization(&c, "108", "pw", "REGISTER", "sip:sip.example", 1, "cn");
        assert!(!a.contains("qop="));
        assert!(a.starts_with(r#"Digest username="108", realm="asterisk""#));
        assert!(parse_challenge(r#"Digest realm="r",nonce="n",algorithm=SHA-256"#).is_err());
    }
}
