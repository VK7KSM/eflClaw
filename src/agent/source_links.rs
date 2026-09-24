//! elfClaw 2026-09-24: keep scheduled-job output from carrying links the model
//! rewrote.
//!
//! The news worker's final text is delivered to the user as-is, and the model
//! "tidies" URLs while summarising: on the first 2026-09-24 run it upper-cased
//! `ic-7300mk2` to `IC-7300MK2` (404) and inserted a `/news/` segment into a
//! Hackaday URL. Asking the model not to do that is not a guarantee, so code
//! checks every link before delivery against the URLs that actually appeared
//! in tool results during the run:
//!
//! - `with_ledger` runs a future with a task-local ledger; `record` (called by
//!   the agent loop for every tool result) adds the URLs it finds to it.
//! - `enforce` keeps links that match a recorded URL exactly, swaps a link
//!   that differs only in case / `www.` / trailing slash, or by extra path
//!   segments around the same slug, for the recorded original, and drops any
//!   other link (a markdown link keeps its text, a bare URL is removed).

use regex::Regex;
use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::sync::LazyLock;

tokio::task_local! {
    static LEDGER: RefCell<HashSet<String>>;
}

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>()\[\]{}\\`]+"#).expect("valid regex"));
static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]\n]*)\]\((https?://[^)\s]+)\)").expect("valid regex"));

/// Characters that end a sentence rather than a URL.
const TRAILING_PUNCT: &[char] = &[
    '.', ',', ';', ':', '!', '?', '\'', '"', '。', '，', '；', '：',
];

/// Run `fut` with a fresh ledger and return its output together with every URL
/// recorded while it ran.
pub async fn with_ledger<F: Future>(fut: F) -> (F::Output, HashSet<String>) {
    LEDGER
        .scope(RefCell::new(HashSet::new()), async move {
            let output = fut.await;
            let urls = LEDGER.with(RefCell::take);
            (output, urls)
        })
        .await
}

/// Record the URLs in `text` (a tool result) into the current ledger. No-op
/// outside `with_ledger` (interactive chats, tests).
pub fn record(text: &str) {
    let _ = LEDGER.try_with(|ledger| ledger.borrow_mut().extend(urls_in(text)));
}

/// Every URL in `text`, trailing sentence punctuation removed, `&amp;` decoded.
pub fn urls_in(text: &str) -> HashSet<String> {
    URL_RE
        .find_iter(text)
        .map(|m| normalize_raw(m.as_str()))
        .collect()
}

fn normalize_raw(url: &str) -> String {
    url.trim_end_matches(TRAILING_PUNCT).replace("&amp;", "&")
}

/// Case-insensitive identity: no scheme, no `www.`, no trailing slash.
fn loose_key(url: &str) -> String {
    let lower = url.to_lowercase();
    let without_scheme = lower
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    without_scheme
        .trim_start_matches("www.")
        .trim_end_matches('/')
        .to_string()
}

/// (host, path segments) of a loose key.
fn split_key(key: &str) -> (&str, Vec<&str>) {
    let (host, path) = key.split_once('/').unwrap_or((key, ""));
    let path = path.split(['?', '#']).next().unwrap_or("");
    (host, path.split('/').filter(|s| !s.is_empty()).collect())
}

/// True when `inner` appears in `outer` in order (not necessarily adjacent).
fn is_subsequence(inner: &[&str], outer: &[&str]) -> bool {
    let mut it = outer.iter();
    inner.iter().all(|seg| it.any(|o| o == seg))
}

/// The recorded URL `url` stands for, if any.
fn resolve(url: &str, known: &HashSet<String>) -> Option<String> {
    if known.contains(url) {
        return Some(url.to_string());
    }
    let key = loose_key(url);
    let mut same: Vec<&String> = known.iter().filter(|k| loose_key(k) == key).collect();
    if same.len() == 1 {
        return same.pop().cloned();
    }
    // Same host and same final slug, the recorded path being the model's path
    // with segments removed (the model inserted e.g. `/news/`) — or the other
    // way around. Only an unambiguous match is used.
    let (host, segs) = split_key(&key);
    let slug = segs.last()?;
    let mut near: Vec<&String> = known
        .iter()
        .filter(|k| {
            let k_key = loose_key(k);
            let (k_host, k_segs) = split_key(&k_key);
            k_host == host
                && k_segs.last() == Some(slug)
                && (is_subsequence(&k_segs, &segs) || is_subsequence(&segs, &k_segs))
        })
        .collect();
    if near.len() == 1 {
        return near.pop().cloned();
    }
    None
}

/// Result of `enforce`.
#[derive(Debug, Default)]
pub struct LinkCheck {
    pub output: String,
    /// (link as written by the model, recorded URL it was replaced with)
    pub repaired: Vec<(String, String)>,
    /// Links with no recorded counterpart, removed from the output.
    pub removed: Vec<String>,
}

/// Rewrite `output` so every link in it is one of `known` (see module docs).
pub fn enforce(output: &str, known: &HashSet<String>) -> LinkCheck {
    let mut check = LinkCheck::default();

    // Markdown links first; stash the results so the bare-URL pass below
    // doesn't touch them.
    let mut stashed: Vec<String> = Vec::new();
    let with_placeholders = MD_LINK_RE.replace_all(output, |caps: &regex::Captures| {
        let text = &caps[1];
        let url = normalize_raw(&caps[2]);
        let replacement = match resolve(&url, known) {
            Some(good) => {
                if good != url {
                    check.repaired.push((url.clone(), good.clone()));
                }
                format!("[{text}]({good})")
            }
            None => {
                check.removed.push(url.clone());
                text.to_string()
            }
        };
        stashed.push(replacement);
        format!("\u{0}{}\u{0}", stashed.len() - 1)
    });

    let bare_done = URL_RE.replace_all(&with_placeholders, |caps: &regex::Captures| {
        let raw = &caps[0];
        let trimmed = raw.trim_end_matches(TRAILING_PUNCT);
        let suffix = &raw[trimmed.len()..];
        let url = trimmed.replace("&amp;", "&");
        match resolve(&url, known) {
            Some(good) => {
                if good != url {
                    check.repaired.push((url.clone(), good.clone()));
                }
                format!("{good}{suffix}")
            }
            None => {
                check.removed.push(url.clone());
                suffix.to_string()
            }
        }
    });

    let mut restored = String::with_capacity(bare_done.len());
    let mut parts = bare_done.split('\u{0}');
    if let Some(first) = parts.next() {
        restored.push_str(first);
    }
    while let (Some(index), Some(rest)) = (parts.next(), parts.next()) {
        match index.parse::<usize>().ok().and_then(|i| stashed.get(i)) {
            Some(link) => restored.push_str(link),
            None => restored.push_str(index),
        }
        restored.push_str(rest);
    }
    check.output = restored;
    check
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(urls: &[&str]) -> HashSet<String> {
        urls.iter().map(|u| (*u).to_string()).collect()
    }

    #[test]
    fn exact_links_are_kept_unchanged() {
        let k = known(&[
            "https://hackaday.com/2026/09/24/reconstructing-device-firmware-from-spi-reads/",
        ]);
        let out = "• [Firmware](https://hackaday.com/2026/09/24/reconstructing-device-firmware-from-spi-reads/) — 说明";
        let check = enforce(out, &k);
        assert_eq!(check.output, out);
        assert!(check.repaired.is_empty() && check.removed.is_empty());
    }

    #[test]
    fn case_changed_link_is_restored_from_the_source() {
        // Real 2026-09-24 case: the model upper-cased the slug → 404.
        let k = known(&["https://www.icqpodcast.com/news/2026/9/20/icom-releases-firmware-update-103-for-the-ic-7300mk2"]);
        let out = "• [Icom](https://www.icqpodcast.com/news/2026/9/20/icom-releases-firmware-update-103-for-the-ic-7300MK2) — 固件";
        let check = enforce(out, &k);
        assert_eq!(
            check.output,
            "• [Icom](https://www.icqpodcast.com/news/2026/9/20/icom-releases-firmware-update-103-for-the-ic-7300mk2) — 固件"
        );
        assert_eq!(check.repaired.len(), 1);
    }

    #[test]
    fn inserted_path_segment_is_restored_from_the_source() {
        // Real 2026-09-24 case: the model inserted `/news/`.
        let k = known(&[
            "https://hackaday.com/2026/09/23/audio-spectrum-analyzer-on-an-esp32-display-board/",
        ]);
        let out = "[ESP32](https://hackaday.com/news/2026/09/23/audio-spectrum-analyzer-on-an-esp32-display-board/)";
        let check = enforce(out, &k);
        assert_eq!(
            check.output,
            "[ESP32](https://hackaday.com/2026/09/23/audio-spectrum-analyzer-on-an-esp32-display-board/)"
        );
    }

    #[test]
    fn unknown_links_are_dropped_keeping_markdown_text() {
        let k = known(&["https://a.example.com/post-1"]);
        let out =
            "• [Made up](https://b.example.com/fake-story) — 说明\n见 https://c.example.com/x。";
        let check = enforce(out, &k);
        assert_eq!(check.output, "• Made up — 说明\n见 。");
        assert_eq!(check.removed.len(), 2);
    }

    #[test]
    fn ambiguous_near_matches_are_not_guessed() {
        let k = known(&[
            "https://site.example.com/a/story",
            "https://site.example.com/b/story",
        ]);
        let check = enforce("[x](https://site.example.com/story)", &k);
        assert_eq!(check.output, "x");
        assert_eq!(
            check.removed,
            vec!["https://site.example.com/story".to_string()]
        );
    }

    #[test]
    fn bare_urls_keep_trailing_punctuation_and_amp_is_decoded() {
        let k = known(&["https://a.example.com/p?x=1&y=2"]);
        let check = enforce("原文：https://a.example.com/p?x=1&amp;y=2。", &k);
        assert_eq!(check.output, "原文：https://a.example.com/p?x=1&y=2。");
    }

    #[test]
    fn urls_in_extracts_from_json_tool_output() {
        let tool_output = r#"{"success":true,"items":[{"url":"https://a.example.com/one","title":"t"}],"markdown":"see https://b.example.com/two."}"#;
        let urls = urls_in(tool_output);
        assert!(urls.contains("https://a.example.com/one"));
        assert!(urls.contains("https://b.example.com/two"));
    }

    #[tokio::test]
    async fn ledger_collects_urls_recorded_inside_the_scope_only() {
        record("https://outside.example.com/ignored");
        let ((), urls) = with_ledger(async {
            record("tool said https://a.example.com/one");
            let nested = async { record("and https://b.example.com/two") };
            futures_util::future::join_all(vec![nested]).await;
        })
        .await;
        assert_eq!(
            urls,
            known(&["https://a.example.com/one", "https://b.example.com/two"])
        );
    }
}
