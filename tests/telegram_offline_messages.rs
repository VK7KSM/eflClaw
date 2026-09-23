//! Regression test for Telegram messages sent while the daemon was offline.
//!
//! Bug: `TelegramChannel::listen()` claims the `getUpdates` polling slot with
//! a `timeout=0` "startup probe" before entering the real long-poll loop
//! (to avoid a 409 conflict if a previous daemon instance's connection is
//! still active). The probe used to read every `update_id` out of its
//! response just to advance the in-memory `offset` past them — discarding
//! the message content entirely. Any message sent while the bot was
//! offline/restarting was silently dropped: the user would see no reply and
//! no error, and the message was gone (Telegram's `getUpdates` doesn't keep
//! serving an update forever — once a *later* call passes a higher offset,
//! it won't come back).
//!
//! Fix: the probe no longer touches `offset`. `getUpdates` doesn't consume
//! updates just by returning them, so the main long-poll loop's first
//! request reuses the same offset, gets the identical pending update back,
//! and processes it through the real parse chain this time.

use wiremock::matchers::{body_partial_json, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroclaw::channels::telegram::TelegramChannel;
use zeroclaw::channels::traits::Channel;

fn pending_update(update_id: i64, text: &str) -> serde_json::Value {
    serde_json::json!({
        "update_id": update_id,
        "message": {
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 555, "is_bot": false, "first_name": "Test"},
            "text": text
        }
    })
}

#[tokio::test]
async fn message_sent_while_offline_is_processed_not_dropped() {
    let server = MockServer::start().await;

    // Simulate real Telegram semantics: a request with offset=0 (the initial
    // value, used by both the startup probe and — with the fix — the main
    // loop's first real call too) gets the pending update back. Any other
    // offset (what the *buggy* code would send after wrongly advancing past
    // it in the probe) gets an empty result, exactly like the real API once
    // an update has been acknowledged past. This is what makes the test
    // actually distinguish "processed for real" from "silently discarded":
    // with the bug, the probe consumes update_id 100 at offset=0, and the
    // main loop's follow-up call (offset=101) would get nothing back.
    Mock::given(method("POST"))
        .and(path_regex(r"/botTEST_TOKEN/getUpdates$"))
        .and(body_partial_json(serde_json::json!({"offset": 0})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": [pending_update(100, "hello from before restart")]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"/botTEST_TOKEN/getUpdates$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": []
        })))
        .mount(&server)
        .await;

    // register_commands() is called unconditionally at the start of listen();
    // let it fail harmlessly against the mock (the code only logs a warning).

    let channel = TelegramChannel::new("TEST_TOKEN".into(), vec!["*".into()], false, false)
        .with_api_base(server.uri());

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        let _ = channel.listen(tx).await;
    });

    let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for the offline message to be delivered")
        .expect("channel closed without delivering the offline message");

    assert_eq!(received.content, "hello from before restart");
    assert_eq!(received.sender, "555");
}
