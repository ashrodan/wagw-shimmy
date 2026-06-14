//! End-to-end mapping tests against in-process mock GOWA + mock agent servers.
//!
//! Proves the spec's hard correctness points without any real WhatsApp account or network:
//! - inbound HMAC verification (raw bytes), ack-fast + async forward, id dedup;
//! - the chat_id round-trip (group `…@g.us` is forwarded verbatim, not rewritten to the sender);
//! - outbound bearer enforcement, JID pass-through to GOWA, and reply-to-bot mention detection.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex};
use tower::ServiceExt;

use wagw_shimmy::{
    AppState, build_router,
    config::{Config, DmPolicy, GroupPolicy, PolicyConfig},
    gowa::sign,
};

const WEBHOOK_SECRET: &str = "tenant-webhook-secret";
const WEBHOOK_BEARER: &str = "agent-inbound-bearer";
const GATEWAY_BEARER: &str = "agent-to-shim-bearer";

/// Captured requests a mock server has seen.
#[derive(Clone, Default)]
struct Recorder {
    bodies: Arc<Mutex<Vec<Value>>>,
    count: Arc<AtomicUsize>,
    /// Optional artificial delay before the mock responds (simulates a slow agent turn).
    delay: Arc<Mutex<Duration>>,
}

impl Recorder {
    async fn record(&self, body: Value) {
        let delay = *self.delay.lock().await;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.bodies.lock().await.push(body);
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    fn hits(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

/// Spawn a mock agent (`POST /whatsapp/inbound`) on an ephemeral port. Records the forwarded body
/// and asserts the bearer; returns its base URL and the recorder.
async fn spawn_mock_agent() -> (String, Recorder) {
    let recorder = Recorder::default();
    let app = Router::new()
        .route(
            "/whatsapp/inbound",
            post(
                |State(rec): State<Recorder>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    let ok = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        == Some(&format!("Bearer {WEBHOOK_BEARER}"));
                    if !ok {
                        return StatusCode::UNAUTHORIZED;
                    }
                    rec.record(body).await;
                    StatusCode::OK
                },
            ),
        )
        .with_state(recorder.clone());
    let addr = serve(app).await;
    (format!("http://{addr}"), recorder)
}

/// Spawn a mock GOWA (`POST /send/message`) that records the send body and returns a message id.
async fn spawn_mock_gowa(message_id: &'static str) -> (String, Recorder) {
    let recorder = Recorder::default();
    let app = Router::new()
        .route(
            "/send/message",
            post(
                move |State(rec): State<Recorder>, Json(body): Json<Value>| async move {
                    rec.record(body).await;
                    Json(json!({ "code": "SUCCESS", "results": { "message_id": message_id } }))
                },
            ),
        )
        .with_state(recorder.clone());
    let addr = serve(app).await;
    (format!("http://{addr}"), recorder)
}

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn test_config(gowa_url: &str, agent_base: &str) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        gowa_url: gowa_url.trim_end_matches('/').to_string(),
        gowa_basic_auth: None,
        gowa_device_id: "61400000000:1@s.whatsapp.net".into(),
        gowa_webhook_secret: WEBHOOK_SECRET.into(),
        agent_inbound_url: format!("{}/whatsapp/inbound", agent_base.trim_end_matches('/')),
        whatsapp_webhook_token: WEBHOOK_BEARER.into(),
        whatsapp_gateway_token: GATEWAY_BEARER.into(),
        policy: PolicyConfig {
            dm_policy: DmPolicy::Open,
            dm_allow: vec![],
            group_policy: GroupPolicy::Open,
            group_allow: vec![],
            require_mention: true,
            free_response_chats: vec![],
        },
        send_rate_per_min: 1000,
    }
}

async fn state_for(gowa_url: &str, agent_base: &str) -> AppState {
    AppState::new(Arc::new(test_config(gowa_url, agent_base))).unwrap()
}

/// POST a signed GOWA webhook through the shim router; returns the HTTP status.
async fn post_webhook(router: &Router, raw: &str) -> StatusCode {
    let signature = sign(WEBHOOK_SECRET.as_bytes(), raw.as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/webhook/gowa")
        .header("content-type", "application/json")
        .header("x-hub-signature-256", signature)
        .body(Body::from(raw.to_string()))
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Poll until the recorder reaches `want` hits or the deadline passes.
async fn wait_for_hits(rec: &Recorder, want: usize) {
    for _ in 0..100 {
        if rec.hits() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn inbound_dm_is_forwarded_with_contract_body() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    assert_eq!(
        post_webhook(&router, &fixture("dm_text.json")).await,
        StatusCode::OK
    );
    wait_for_hits(&agent, 1).await;

    let bodies = agent.bodies.lock().await;
    assert_eq!(bodies.len(), 1);
    let forwarded = &bodies[0];
    // Only the four-field contract is forwarded — and chat_id is the conversation JID verbatim.
    assert_eq!(forwarded["chat_id"], "61400111222@s.whatsapp.net");
    assert_eq!(forwarded["body"], "hello from a dm");
    assert_eq!(forwarded["id"], "MSG_DM_1");
    assert_eq!(forwarded["from_me"], false);
    assert!(
        forwarded.get("sender").is_none(),
        "internal-only fields must not leak"
    );
}

#[tokio::test]
async fn inbound_bad_signature_is_rejected_and_not_forwarded() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    let raw = fixture("dm_text.json");
    let request = Request::builder()
        .method("POST")
        .uri("/webhook/gowa")
        .header("x-hub-signature-256", "sha256=deadbeef")
        .body(Body::from(raw))
        .unwrap();
    let status = router.clone().oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        agent.hits(),
        0,
        "a forged webhook must never reach the agent"
    );
}

#[tokio::test]
async fn duplicate_delivery_forwards_once() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    let raw = fixture("dm_text.json");
    assert_eq!(post_webhook(&router, &raw).await, StatusCode::OK);
    assert_eq!(post_webhook(&router, &raw).await, StatusCode::OK); // GOWA re-delivery
    wait_for_hits(&agent, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(agent.hits(), 1, "same id must be forwarded at most once");
}

#[tokio::test]
async fn from_me_echo_is_dropped() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    assert_eq!(
        post_webhook(&router, &fixture("from_me_echo.json")).await,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(agent.hits(), 0);
}

#[tokio::test]
async fn plain_group_message_dropped_under_require_mention() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    // No reply-to-bot, require_mention=true → silently dropped (acked, not forwarded).
    assert_eq!(
        post_webhook(&router, &fixture("group_text_plain.json")).await,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(agent.hits(), 0);
}

#[tokio::test]
async fn ack_is_fast_even_when_agent_is_slow() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    // Agent takes 2s to "process" — the webhook ack must not wait for it.
    *agent.delay.lock().await = Duration::from_secs(2);
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    let started = std::time::Instant::now();
    assert_eq!(
        post_webhook(&router, &fixture("dm_text.json")).await,
        StatusCode::OK
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "ack must return before the slow agent finishes (took {:?})",
        started.elapsed()
    );
}

#[tokio::test]
async fn reply_to_bot_summons_in_require_mention_group() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, gowa) = spawn_mock_gowa("OUT_BOT_1").await;
    let state = state_for(&gowa_url, &agent_url).await;
    let router = build_router(state);

    // 1. Bot sends a message into the group; GOWA returns id OUT_BOT_1, which the shim records.
    let send = Request::builder()
        .method("POST")
        .uri("/send")
        .header("authorization", format!("Bearer {GATEWAY_BEARER}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"chat_id":"120363000000000000@g.us","text":"hi group"}).to_string(),
        ))
        .unwrap();
    let resp = router.clone().oneshot(send).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    wait_for_hits(&gowa, 1).await;
    {
        let bodies = gowa.bodies.lock().await;
        // The group JID is passed straight through as GOWA's `phone`.
        assert_eq!(bodies[0]["phone"], "120363000000000000@g.us");
        assert_eq!(bodies[0]["message"], "hi group");
    }

    // 2. A group member replies to that message (replied_to_id = OUT_BOT_1) → counts as a mention,
    //    so it is forwarded despite require_mention=true.
    assert_eq!(
        post_webhook(&router, &fixture("group_text_reply_to_bot.json")).await,
        StatusCode::OK
    );
    wait_for_hits(&agent, 1).await;
    let bodies = agent.bodies.lock().await;
    assert_eq!(bodies.len(), 1);
    // The reply is answered IN THE GROUP — chat_id is the group JID, not the sender's DM JID.
    assert_eq!(bodies[0]["chat_id"], "120363000000000000@g.us");
}

#[tokio::test]
async fn send_requires_bearer() {
    let (agent_url, _agent) = spawn_mock_agent().await;
    let (gowa_url, gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    let send = Request::builder()
        .method("POST")
        .uri("/send")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"chat_id":"x@s.whatsapp.net","text":"hi"}).to_string(),
        ))
        .unwrap();
    let status = router.clone().oneshot(send).await.unwrap().status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(gowa.hits(), 0, "unauthenticated send must not reach GOWA");
}

#[tokio::test]
async fn oversized_body_is_rejected() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);

    // 1 MiB body — above the 256 KiB cap. Must be rejected (413) and never forwarded.
    let huge = "x".repeat(1024 * 1024);
    let signature = sign(WEBHOOK_SECRET.as_bytes(), huge.as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/webhook/gowa")
        .header("x-hub-signature-256", signature)
        .body(Body::from(huge))
        .unwrap();
    let status = router.clone().oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(agent.hits(), 0);
}

#[tokio::test]
async fn healthz_ok() {
    let (agent_url, _a) = spawn_mock_agent().await;
    let (gowa_url, _g) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);
    let request = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
