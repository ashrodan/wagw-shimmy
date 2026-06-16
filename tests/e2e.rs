//! End-to-end mapping tests against in-process mock GOWA + mock agent servers.
//!
//! Proves the spec's hard correctness points without any real WhatsApp account or network:
//! - inbound HMAC verification (raw bytes), ack-fast + async forward, id dedup;
//! - the chat_id round-trip (group `…@g.us` is forwarded verbatim, not rewritten to the sender);
//! - outbound bearer enforcement, JID pass-through to GOWA, and reply-to-bot mention detection.

use std::{
    net::SocketAddr,
    path::PathBuf,
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
    channel::ChannelConfig,
    config::{Config, DmPolicy, GroupPolicy, PolicyConfig},
    gowa::sign,
    model::Inbound,
};

const WEBHOOK_SECRET: &str = "tenant-webhook-secret";
const WEBHOOK_BEARER: &str = "agent-inbound-bearer";
const GATEWAY_BEARER: &str = "agent-to-shim-bearer";
/// A distinct bearer for the per-group channel `support`, to prove the right credential is sent.
const CHANNEL_A_BEARER: &str = "channel-support-bearer";

/// Captured requests a mock server has seen.
#[derive(Clone, Default)]
struct Recorder {
    bodies: Arc<Mutex<Vec<Value>>>,
    count: Arc<AtomicUsize>,
    /// Optional artificial delay before the mock responds (simulates a slow agent turn).
    delay: Arc<Mutex<Duration>>,
    /// Number of requests the mock agent should reject with 500 before it starts succeeding
    /// (simulates a flaky/recovering agent). Decrements per failed request.
    fail_remaining: Arc<AtomicUsize>,
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

/// Spawn a mock agent (`POST /whatsapp/inbound`) on an ephemeral port asserting the default bearer.
async fn spawn_mock_agent() -> (String, Recorder) {
    spawn_mock_agent_bearer(WEBHOOK_BEARER).await
}

/// Spawn a mock agent that asserts a specific bearer (so per-channel bearers can be distinguished).
/// Records the forwarded body; returns its base URL and the recorder.
async fn spawn_mock_agent_bearer(expected_bearer: &'static str) -> (String, Recorder) {
    let recorder = Recorder::default();
    let app =
        Router::new()
            .route(
                "/whatsapp/inbound",
                post(
                    move |State(rec): State<Recorder>,
                          headers: HeaderMap,
                          Json(body): Json<Value>| async move {
                        let ok = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            == Some(&format!("Bearer {expected_bearer}"));
                        if !ok {
                            return StatusCode::UNAUTHORIZED;
                        }
                        // Simulate a flaky agent: reject the first `fail_remaining` requests with 500.
                        if rec.fail_remaining.load(Ordering::SeqCst) > 0 {
                            rec.fail_remaining.fetch_sub(1, Ordering::SeqCst);
                            return StatusCode::INTERNAL_SERVER_ERROR;
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

/// Spawn a mock GOWA (`POST /send/message` + `GET /devices`) that records the send body and returns
/// a message id. `GET /devices` answers 200 so the shim's `/readyz` GOWA probe passes.
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
        .route(
            "/devices",
            axum::routing::get(|| async {
                Json(json!({ "results": [{ "jid": "61400000000:1@s.whatsapp.net" }] }))
            }),
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

/// A unique, never-cleaned queue dir per call (the OS reaps `/tmp`). Avoids a tempfile dependency
/// and keeps concurrent tests from sharing a queue.
fn unique_queue_dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("wagw-e2e-{}-{}", std::process::id(), n))
}

/// A `ChannelConfig` for a base URL with the given label + bearer (mirrors the renderer's URL shape).
fn channel(label: &str, base: &str, bearer: &str) -> ChannelConfig {
    let base = base.trim_end_matches('/');
    ChannelConfig {
        label: label.into(),
        inbound_url: format!("{base}/whatsapp/inbound"),
        health_url: format!("{base}/health"),
        bearer: bearer.into(),
    }
}

fn test_config(gowa_url: &str, agent_base: &str) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        gowa_url: gowa_url.trim_end_matches('/').to_string(),
        gowa_basic_auth: None,
        gowa_device_id: "61400000000:1@s.whatsapp.net".into(),
        gowa_webhook_secret: WEBHOOK_SECRET.into(),
        agent_inbound_url: format!("{}/whatsapp/inbound", agent_base.trim_end_matches('/')),
        agent_health_url: format!("{}/health", agent_base.trim_end_matches('/')),
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
        readyz_probe_agent: false,
        agent_debug_sink: false,
        queue_dir: unique_queue_dir(),
        // Few retries + a tiny backoff so dead-letter/retry tests finish in milliseconds.
        forward_max_retries: 3,
        forward_concurrency: 4,
        forward_backoff: Duration::from_millis(10),
        // Default-only routing (matches a config with no WA_CHANNELS): one channel = today's target.
        channels: vec![channel("default", agent_base, WEBHOOK_BEARER)],
        group_channels: vec![],
        self_number: None,
    }
}

async fn state_for(gowa_url: &str, agent_base: &str) -> AppState {
    AppState::new(Arc::new(test_config(gowa_url, agent_base))).unwrap()
}

/// Build an `Inbound` for tests that drive the forward queue directly.
fn test_inbound(chat_id: &str, body: &str, id: &str) -> Inbound {
    Inbound {
        chat_id: chat_id.into(),
        sender: chat_id.into(),
        body: body.into(),
        id: id.into(),
        is_from_me: false,
        mentioned: false,
        reply_to: None,
        channel: "default".into(),
    }
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
    let state = state_for(&gowa_url, &agent_url).await;
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

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
    // A DM always routes to the default channel; the label travels with the forward.
    assert_eq!(forwarded["channel"], "default");
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
    let state = state_for(&gowa_url, &agent_url).await;
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

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
async fn at_mention_summons_in_require_mention_group() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;

    // The bot's own number; GOWA rewrites a tag of it into the body as `@61413118079`.
    let mut config = test_config(&gowa_url, &agent_url);
    config.self_number = Some("61413118079".into());
    let state = AppState::new(Arc::new(config)).unwrap();
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

    // A plain (untagged) group message is still dropped under require_mention=true.
    assert_eq!(
        post_webhook(&router, &fixture("group_text_plain.json")).await,
        StatusCode::OK
    );

    // A group message that @-tags the bot is forwarded despite require_mention=true.
    let tagged = json!({
        "event": "message",
        "device_id": "61400000000:1@s.whatsapp.net",
        "payload": {
            "chat_id": "120363000000000000@g.us",
            "from": "61400111222@s.whatsapp.net",
            "body": "@61413118079 what's the weather today?",
            "id": "MSG_GROUP_TAG_1",
            "is_from_me": false
        }
    })
    .to_string();
    assert_eq!(post_webhook(&router, &tagged).await, StatusCode::OK);

    wait_for_hits(&agent, 1).await;
    let bodies = agent.bodies.lock().await;
    assert_eq!(bodies.len(), 1, "only the tagged message is forwarded");
    // Answered IN THE GROUP, and it was the tagged message (not the plain one).
    assert_eq!(bodies[0]["chat_id"], "120363000000000000@g.us");
    assert_eq!(bodies[0]["id"], "MSG_GROUP_TAG_1");
}

#[tokio::test]
async fn ack_is_fast_even_when_agent_is_slow() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    // Agent takes 2s to "process" — the webhook ack must not wait for it.
    *agent.delay.lock().await = Duration::from_secs(2);
    let state = state_for(&gowa_url, &agent_url).await;
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

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
    let _worker = state.spawn_forward_worker();
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

#[tokio::test]
async fn livez_is_ok_without_any_dependency() {
    // Liveness is process-only: it must not touch GOWA or the agent. Point both at dead ports.
    let state = state_for("http://127.0.0.1:1", "http://127.0.0.1:1").await;
    let router = build_router(state);
    let request = Request::builder()
        .uri("/livez")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn readyz_is_ok_when_gowa_is_healthy() {
    let (agent_url, _agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let router = build_router(state_for(&gowa_url, &agent_url).await);
    let request = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn readyz_is_503_when_gowa_is_down() {
    let (agent_url, _agent) = spawn_mock_agent().await;
    // GOWA at a closed port → the /devices probe fails → not ready.
    let router = build_router(state_for("http://127.0.0.1:1", &agent_url).await);
    let request = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn debug_sink_drains_inbound_without_an_agent() {
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    // No agent at all — point the forward target at a dead port. Sink mode must still drain cleanly.
    let mut config = test_config(&gowa_url, "http://127.0.0.1:1");
    config.agent_debug_sink = true;
    let state = AppState::new(Arc::new(config)).unwrap();
    let queue = state.queue.clone();
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

    assert_eq!(
        post_webhook(&router, &fixture("dm_text.json")).await,
        StatusCode::OK
    );

    // The forward is sunk (logged + success), so the pending file is removed and nothing dead-letters.
    for _ in 0..50 {
        if queue.pending_len() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(queue.pending_len(), 0, "sink drains the pending file");
    assert_eq!(queue.dead_len(), 0, "sink never dead-letters");
}

#[tokio::test]
async fn readyz_reports_debug_sink() {
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let mut config = test_config(&gowa_url, "http://127.0.0.1:1");
    config.agent_debug_sink = true;
    let router = build_router(AppState::new(Arc::new(config)).unwrap());
    let request = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["agent"]["debug_sink"], true);
    assert_eq!(value["status"], "ok");
}

#[tokio::test]
async fn forward_retries_then_succeeds_and_clears_pending() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    // Agent rejects the first two forward attempts with 500, then accepts.
    agent.fail_remaining.store(2, Ordering::SeqCst);
    let state = state_for(&gowa_url, &agent_url).await;
    let queue = state.queue.clone();
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

    // Ack is still fast (it doesn't wait on the agent).
    let started = std::time::Instant::now();
    assert_eq!(
        post_webhook(&router, &fixture("dm_text.json")).await,
        StatusCode::OK
    );
    assert!(started.elapsed() < Duration::from_millis(500));

    // The worker retries past the two 500s; the agent ultimately sees exactly one delivery.
    wait_for_hits(&agent, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(agent.hits(), 1, "the id is delivered exactly once");
    assert_eq!(
        queue.pending_len(),
        0,
        "the pending file is removed on success"
    );
    assert_eq!(
        queue.dead_len(),
        0,
        "a recovered forward never dead-letters"
    );
}

#[tokio::test]
async fn forward_dead_letters_when_agent_stays_down() {
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    // Point the agent at a closed port so every forward attempt is refused.
    let state = state_for(&gowa_url, "http://127.0.0.1:1").await;
    let queue = state.queue.clone();
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

    assert_eq!(
        post_webhook(&router, &fixture("dm_text.json")).await,
        StatusCode::OK
    );

    // After bounded retries the message lands in dead/ rather than being silently lost.
    for _ in 0..100 {
        if queue.dead_len() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(queue.dead_len(), 1, "exhausted forward is dead-lettered");
    assert_eq!(
        queue.pending_len(),
        0,
        "the pending file is moved, not duplicated"
    );
}

#[tokio::test]
async fn mapped_group_routes_to_its_channel_and_others_default() {
    // Two downstream agents: a per-group channel `support` (its own bearer) and the default.
    let (support_url, support) = spawn_mock_agent_bearer(CHANNEL_A_BEARER).await;
    let (default_url, default_agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;

    let mut config = test_config(&gowa_url, &default_url);
    // Map the fixture group → channel `support`; everything else falls through to default.
    config.channels = vec![
        channel("default", &default_url, WEBHOOK_BEARER),
        channel("support", &support_url, CHANNEL_A_BEARER),
    ];
    config.group_channels = vec![("120363000000000000@g.us".into(), "support".into())];
    // Forward plain group messages (no mention gate) so the routing is what's under test.
    config.policy.require_mention = false;

    let state = AppState::new(Arc::new(config)).unwrap();
    let _worker = state.spawn_forward_worker();
    let router = build_router(state);

    // 1. A message in the mapped group lands on the `support` agent with channel:"support".
    assert_eq!(
        post_webhook(&router, &fixture("group_text_plain.json")).await,
        StatusCode::OK
    );
    wait_for_hits(&support, 1).await;
    {
        let bodies = support.bodies.lock().await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["chat_id"], "120363000000000000@g.us");
        assert_eq!(bodies[0]["channel"], "support");
    }
    // It must NOT have reached the default agent.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(default_agent.hits(), 0, "mapped group must not hit default");

    // 2. An unmapped DM lands on the default agent with channel:"default".
    assert_eq!(
        post_webhook(&router, &fixture("dm_text.json")).await,
        StatusCode::OK
    );
    wait_for_hits(&default_agent, 1).await;
    let bodies = default_agent.bodies.lock().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["chat_id"], "61400111222@s.whatsapp.net");
    assert_eq!(bodies[0]["channel"], "default");
    // The mapped channel saw exactly the one group message, not the DM.
    assert_eq!(
        support.hits(),
        1,
        "default traffic must not hit the mapped channel"
    );
}

#[tokio::test]
async fn startup_drain_processes_a_preseeded_pending_file() {
    let (agent_url, agent) = spawn_mock_agent().await;
    let (gowa_url, _gowa) = spawn_mock_gowa("OUT_1").await;
    let state = state_for(&gowa_url, &agent_url).await;
    let queue = state.queue.clone();

    // Pre-seed a pending message BEFORE the worker exists — a stand-in for a file left by a crash.
    queue
        .enqueue(&test_inbound(
            "61400111222@s.whatsapp.net",
            "left over",
            "PRESEED_1",
        ))
        .unwrap();
    assert_eq!(queue.pending_len(), 1);

    // Starting the worker must drain it on startup.
    let _worker = state.spawn_forward_worker();
    wait_for_hits(&agent, 1).await;

    let bodies = agent.bodies.lock().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["id"], "PRESEED_1");
    assert_eq!(bodies[0]["body"], "left over");
    drop(bodies);
    assert_eq!(queue.pending_len(), 0, "the drained file is removed");
}
