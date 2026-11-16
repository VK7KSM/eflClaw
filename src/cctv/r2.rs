//! elfClaw 2026-11-16: camera pictures go to Telegram through Cloudflare R2.
//! Uploading a picture to Telegram from K5 took 16–92 s (445–852 KB, tested
//! on K5), while putting it on R2 takes under a second and Telegram fetches
//! it from there in about 3 s. The bucket (`cctv-snapshots`, owner-created)
//! is private, deletes objects after a day, and is reached with S3 presigned
//! URLs only: a random object name, and a GET link valid for 5 minutes, long
//! enough for Telegram to fetch it.
//!
//! Credentials come from the user environment variables `R2_ACCESS_KEY_ID`,
//! `R2_SECRET_ACCESS_KEY` and `R2_ENDPOINT` (the account's
//! `https://<account>.r2.cloudflarestorage.com`); without them nothing changes.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::Duration;

const BUCKET: &str = "cctv-snapshots";
/// How long the link handed to Telegram stays valid.
const LINK_SECS: u32 = 300;

#[derive(Clone)]
pub struct R2 {
    access_key: String,
    secret_key: String,
    host: String,
    http: reqwest::Client,
}

impl R2 {
    /// `None` unless all three variables are set.
    pub fn from_env() -> Option<Self> {
        let var = |n: &str| std::env::var(n).ok().filter(|v| !v.trim().is_empty());
        let endpoint = var("R2_ENDPOINT")?;
        let host = endpoint
            .trim()
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string();
        Some(Self {
            access_key: var("R2_ACCESS_KEY_ID")?.trim().to_string(),
            secret_key: var("R2_SECRET_ACCESS_KEY")?.trim().to_string(),
            host,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        })
    }

    /// Store a JPEG under a random name; the link Telegram can fetch it by.
    pub async fn put_jpeg(&self, jpeg: Vec<u8>) -> Result<String> {
        let key = format!("{}.jpg", uuid::Uuid::new_v4().simple());
        let now = Utc::now();
        let resp = self
            .http
            .put(self.presign("PUT", &key, LINK_SECS, now))
            .header("Content-Type", "image/jpeg")
            .body(jpeg)
            .send()
            .await
            // The URL carries the key id and a signature: never logged.
            .map_err(|e| anyhow::anyhow!("R2 unreachable: {}", e.without_url()))?;
        if !resp.status().is_success() {
            bail!("R2 upload failed ({})", resp.status().as_u16());
        }
        Ok(self.presign("GET", &key, LINK_SECS, now))
    }

    /// An S3 (SigV4) presigned URL, region `auto`, payload unsigned.
    fn presign(&self, method: &str, key: &str, expires: u32, now: DateTime<Utc>) -> String {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let day = now.format("%Y%m%d").to_string();
        let scope = format!("{day}/auto/s3/aws4_request");
        let path = format!("/{BUCKET}/{}", uri_encode(key, false));
        let credential = format!("{}/{scope}", self.access_key);
        let query = [
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
            ("X-Amz-Credential", credential.as_str()),
            ("X-Amz-Date", amz_date.as_str()),
            ("X-Amz-Expires", &expires.to_string()),
            ("X-Amz-SignedHeaders", "host"),
        ]
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
        .collect::<Vec<_>>()
        .join("&");
        let request = format!(
            "{method}\n{path}\n{query}\nhost:{}\n\nhost\nUNSIGNED-PAYLOAD",
            self.host
        );
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(request.as_bytes()))
        );
        let mut k = hmac(
            format!("AWS4{}", self.secret_key).as_bytes(),
            day.as_bytes(),
        );
        for part in ["auto", "s3", "aws4_request"] {
            k = hmac(&k, part.as_bytes());
        }
        let signature = hex::encode(hmac(&k, to_sign.as_bytes()));
        format!(
            "https://{}{path}?{query}&X-Amz-Signature={signature}",
            self.host
        )
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// S3's URI encoding: unreserved characters stay, the rest is `%XX`; `/`
/// stays in paths.
fn uri_encode(s: &str, slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b'/' if !slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn r2() -> R2 {
        R2 {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            host: "example-account.r2.cloudflarestorage.com".into(),
            http: reqwest::Client::new(),
        }
    }

    /// Same inputs through the Python signer that K5's live tests used
    /// against the bucket (2026-11-16), which R2 accepted.
    #[test]
    fn presigned_url_matches_the_tested_signer() {
        let now = Utc.with_ymd_and_hms(2026, 11, 16, 1, 2, 3).unwrap();
        let url = r2().presign("GET", "abc.jpg", 300, now);
        assert_eq!(
            url,
            "https://example-account.r2.cloudflarestorage.com/cctv-snapshots/abc.jpg?\
             X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20261116%2Fauto%2Fs3%2Faws4_request\
             &X-Amz-Date=20261116T010203Z&X-Amz-Expires=300&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=f1fba4fa444591eda5dc35af64167377e81d1cc717efa2b11212804cdab813e1"
        );
    }

    #[test]
    fn uri_encoding_keeps_unreserved_only() {
        assert_eq!(uri_encode("a/b c+~", true), "a%2Fb%20c%2B~");
        assert_eq!(uri_encode("a/b", false), "a/b");
    }
}
