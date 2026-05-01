//! Canonical mock Home Assistant WebSocket server — single shared harness.
//!
//! This is the **single canonical** mock harness for every Phase 2 test binary
//! (integration, soak, smoke) and the churn bench.  A second mock WS
//! implementation is forbidden; tests MUST drive [`MockWsServer`] rather than
//! rolling their own.
//!
//! # History
//!
//! TASK-035 introduced the original harness under `tests/integration/`.  TASK-039
//! could not modify that path (it was in `must_not_touch`) and produced a
//! near-identical fork under `tests/common/` with one extra method
//! ([`MockWsServer::force_disconnect`]).  TASK-042 unified the two by promoting
//! `tests/common/mock_ws.rs` to the single canonical location and merging in the
//! frame-recording / auth-invalid / mid-session-auth features that were only in
//! the integration copy.  The integration-tree copy was deleted in the same
//! task.
//!
//! # Design
//!
//! - One [`MockWsServer`] binds to an ephemeral local port and accepts any
//!   number of concurrent client connections.  Each accepted connection runs
//!   in its own Tokio task and consults the server's shared state for what to
//!   send next.
//! - A FIFO `scripted_replies` queue holds frames the server will send in order
//!   in response to client requests (e.g. `auth_required` upon connect, then
//!   `auth_ok` after the client sends `auth`, etc.).
//! - The server records every inbound frame (with arrival timestamp) into a
//!   `recorded_frames` log so tests can assert message-receipt ordering — which
//!   is critical for the "subscribe-ACK before snapshot" gate (TASK-029).
//! - Tests can imperatively inject events mid-session via [`MockWsServer::inject_event`]
//!   and force a connection drop via [`MockWsServer::force_disconnect`].
//!
//! # No external new dependency required
//!
//! `tokio-tungstenite` is already a project dependency; both `connect_async`
//! (client side) and `accept_async` (server side) are available with the
//! existing feature set.  `futures` is already a project dependency for its
//! `SinkExt`/`StreamExt` traits.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Recorded frame
// ---------------------------------------------------------------------------

/// A single inbound frame captured by the mock server.
///
/// `kind` is the value of the JSON `"type"` field (e.g. `"auth"`,
/// `"subscribe_events"`, `"get_states"`); `body` is the raw text frame so tests
/// can inspect ids and other fields.  `seq` is a monotonic counter assigned at
/// receipt time, used by the subscribe-ACK ordering test to assert receipt
/// order across two different message kinds.  `received_at` is the wall-clock
/// instant at which the mock pulled the frame off the WebSocket; the
/// subscribe-ACK timestamp test (TASK-046 finding 6) compares this against
/// [`MockWsServer::subscribe_ack_sent_at`] to prove the client's `get_states`
/// frame was sent strictly AFTER the mock's ACK send completed (not merely
/// recorded out-of-order at the seq layer).
#[derive(Debug, Clone)]
pub struct RecordedFrame {
    pub seq: u64,
    pub kind: String,
    pub body: String,
    pub received_at: Instant,
}

// ---------------------------------------------------------------------------
// Scripted reply
// ---------------------------------------------------------------------------

/// A frame the mock server will send to the client.
///
/// `Immediate` frames are sent unconditionally as soon as the client connects.
/// `OnRequest` frames are sent in response to a specific inbound message kind
/// (matched on the JSON `"type"` field) and copy the inbound `id` into the
/// reply when `forward_id` is true (the standard HA `result` pattern).
#[derive(Debug, Clone)]
pub enum ScriptedReply {
    /// Send this raw text frame immediately on connect.
    Immediate(String),
    /// Wait for a request whose `"type"` matches `match_type`, then send the
    /// reply.  When `forward_id` is true the request's `"id"` is substituted
    /// into the reply at the literal placeholder `{{ID}}` (HA result-correlation
    /// pattern).
    OnRequest {
        match_type: String,
        body: String,
        forward_id: bool,
    },
}

// ---------------------------------------------------------------------------
// Server-side shared state
// ---------------------------------------------------------------------------

/// Shared state accessed by the accept loop and by test code via
/// [`MockWsServer`].  Wrapped in `Arc<Mutex<...>>` because tests may inject or
/// inspect from a different task than the server loop.
struct SharedState {
    /// FIFO queue of scripted replies.  Drained as the server matches them
    /// against incoming requests (or sends them immediately on connect).
    replies: Vec<ScriptedReply>,
    /// Log of every inbound text frame received across all accepted connections.
    recorded: Vec<RecordedFrame>,
    /// Frames to inject mid-session (sent on the next connection's loop tick
    /// after a delay).  Each entry is sent once and removed.
    injected: Vec<String>,
    /// Timestamp at which the mock most recently FINISHED sending a reply that
    /// matched on `subscribe_events`.  Populated by the connection task after
    /// `write.send(...).await` returns successfully — i.e. after the ACK has
    /// been handed to tungstenite for transmission.  TASK-046 finding 6 uses
    /// this to assert that the client's `get_states` frame was received
    /// strictly AFTER the ACK was sent (proving the FSM gates `get_states` on
    /// real ACK arrival, not on optimistic send-and-pretend).
    subscribe_ack_sent_at: Option<Instant>,
}

impl SharedState {
    fn new() -> Self {
        SharedState {
            replies: Vec::new(),
            recorded: Vec::new(),
            injected: Vec::new(),
            subscribe_ack_sent_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// MockWsServer
// ---------------------------------------------------------------------------

/// A scriptable mock Home Assistant WebSocket server.
///
/// Construct via [`MockWsServer::start`] which binds an ephemeral port and
/// returns the bound URL plus the handle.  Drop the handle to shut down the
/// accept loop (the accept-task is aborted in `Drop`).
pub struct MockWsServer {
    /// URL the client should connect to (e.g. `ws://127.0.0.1:54321/api/websocket`).
    pub ws_url: String,
    state: Arc<Mutex<SharedState>>,
    accept_task: JoinHandle<()>,
    /// When `true` the connection loop sends a Close frame on the next tick and
    /// resets the flag.  Set via [`MockWsServer::force_disconnect`].
    disconnect_flag: Arc<AtomicBool>,
}

impl MockWsServer {
    /// Bind to an ephemeral local port and start the accept loop.
    ///
    /// Returns once the listener is bound (so `ws_url` is valid for clients to
    /// connect to immediately).
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock WS server: bind ephemeral port");
        let local_addr = listener
            .local_addr()
            .expect("mock WS server: read local_addr");
        let ws_url = format!("ws://{}/api/websocket", local_addr);

        let state = Arc::new(Mutex::new(SharedState::new()));
        let seq_counter = Arc::new(AtomicU32::new(0));
        let disconnect_flag = Arc::new(AtomicBool::new(false));

        let accept_state = Arc::clone(&state);
        let accept_flag = Arc::clone(&disconnect_flag);
        let accept_task = tokio::spawn(async move {
            run_accept_loop(listener, accept_state, seq_counter, accept_flag).await;
        });

        MockWsServer {
            ws_url,
            state,
            accept_task,
            disconnect_flag,
        }
    }

    /// Append a scripted reply to the FIFO queue.
    ///
    /// Public helpers (`script_auth_ok`, `script_subscribe_ack`, etc.) wrap this
    /// for the common HA flows.
    pub async fn push_reply(&self, reply: ScriptedReply) {
        self.state.lock().await.replies.push(reply);
    }

    /// Script the standard HA auth handshake: `auth_required` (immediate) then
    /// `auth_ok` on receipt of `auth`.
    pub async fn script_auth_ok(&self) {
        self.push_reply(ScriptedReply::Immediate(
            r#"{"type":"auth_required","ha_version":"2024.4.0"}"#.to_owned(),
        ))
        .await;
        self.push_reply(ScriptedReply::OnRequest {
            match_type: "auth".to_owned(),
            body: r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#.to_owned(),
            forward_id: false,
        })
        .await;
    }

    /// Script `auth_required` (immediate) followed by `auth_invalid` with
    /// the given reason on receipt of `auth`.
    pub async fn script_auth_invalid(&self, message: &str) {
        self.push_reply(ScriptedReply::Immediate(
            r#"{"type":"auth_required","ha_version":"2024.4.0"}"#.to_owned(),
        ))
        .await;
        self.push_reply(ScriptedReply::OnRequest {
            match_type: "auth".to_owned(),
            body: format!(r#"{{"type":"auth_invalid","message":"{message}"}}"#),
            forward_id: false,
        })
        .await;
    }

    /// Script the `subscribe_events` ACK handshake.
    ///
    /// TASK-123 (F7): the FSM now sends THREE filtered `subscribe_events`
    /// frames in sequence (`state_changed`, `service_registered`,
    /// `service_removed`) instead of a single wildcard subscribe.  Each
    /// frame is gated on the previous ACK, so the canonical "script the
    /// subscribe handshake" now queues three identical `OnRequest` replies
    /// — one per inbound subscribe frame.  The mock matches OnRequest by
    /// `match_type` and removes the matched entry, so three queued replies
    /// answer the three subscribe frames in order; reply bodies are
    /// identical because each ACK is `result { id, success: true,
    /// result: null }`.
    ///
    /// Pre-TASK-123 callers issued exactly one `script_subscribe_ack().await`
    /// per accepted connection; the function quietly absorbs the FSM
    /// change here so no caller-side fan-out is required.
    pub async fn script_subscribe_ack(&self) {
        for _ in 0..3 {
            self.push_reply(ScriptedReply::OnRequest {
                match_type: "subscribe_events".to_owned(),
                body: r#"{"type":"result","id":{{ID}},"success":true,"result":null}"#.to_owned(),
                forward_id: true,
            })
            .await;
        }
    }

    /// Script `n` successful subscribe ACKs followed by one failure ACK.
    /// Used to test FSM failure paths for each of the three subscribe phases.
    pub async fn script_subscribe_n_acks_then_fail(&self, n: usize) {
        for _ in 0..n {
            self.push_reply(ScriptedReply::OnRequest {
                match_type: "subscribe_events".to_owned(),
                body: r#"{"type":"result","id":{{ID}},"success":true,"result":null}"#.to_owned(),
                forward_id: true,
            })
            .await;
        }
        self.push_reply(ScriptedReply::OnRequest {
            match_type: "subscribe_events".to_owned(),
            body: r#"{"type":"result","id":{{ID}},"success":false,"error":{"code":"unknown_error","message":"subscribe failed"}}"#.to_owned(),
            forward_id: true,
        })
        .await;
    }

    /// Script a `get_states` reply with the given JSON entity array.
    ///
    /// `entities_json` is the literal JSON array string for the `result` field
    /// (e.g. `r#"[{"entity_id":"light.x","state":"on", ...}]"#`).
    pub async fn script_get_states_reply(&self, entities_json: &str) {
        let body = format!(
            r#"{{"type":"result","id":{{{{ID}}}},"success":true,"result":{entities_json}}}"#
        );
        self.push_reply(ScriptedReply::OnRequest {
            match_type: "get_states".to_owned(),
            body,
            forward_id: true,
        })
        .await;
    }

    /// Script a `get_services` reply with the given JSON service map.
    pub async fn script_get_services_reply(&self, services_json: &str) {
        let body = format!(
            r#"{{"type":"result","id":{{{{ID}}}},"success":true,"result":{services_json}}}"#
        );
        self.push_reply(ScriptedReply::OnRequest {
            match_type: "get_services".to_owned(),
            body,
            forward_id: true,
        })
        .await;
    }

    /// Inject a raw text frame to be sent to the connected client on the next
    /// loop tick.  Used to fire `state_changed` events mid-session.
    pub async fn inject_event(&self, raw: String) {
        self.state.lock().await.injected.push(raw);
    }

    /// Inject a batch of raw text frames in one shared-state lock acquisition.
    ///
    /// Equivalent to calling [`MockWsServer::inject_event`] in a loop, but
    /// holds the inject queue's mutex once for the whole batch.  Used by the
    /// snapshot-buffer overflow integration test (TASK-046 finding 7) which
    /// must shovel >`PROFILE_DESKTOP.snapshot_buffer_events` (10 000) frames
    /// at the FSM during `Phase::Snapshotting`; per-event locking would
    /// add ~10 000 mutex acquisitions for no benefit.
    pub async fn inject_events_batch<I: IntoIterator<Item = String>>(&self, raws: I) {
        let mut s = self.state.lock().await;
        s.injected.extend(raws);
    }

    /// Inject a mid-session `auth_required` re-prompt.
    pub async fn inject_auth_required(&self) {
        self.inject_event(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#.to_owned())
            .await;
    }

    /// Snapshot of all recorded inbound frames in receipt order.
    pub async fn recorded_requests(&self) -> Vec<RecordedFrame> {
        self.state.lock().await.recorded.clone()
    }

    /// Count of recorded frames matching the given `"type"` value.
    pub async fn recorded_request_count(&self, kind: &str) -> usize {
        self.state
            .lock()
            .await
            .recorded
            .iter()
            .filter(|r| r.kind == kind)
            .count()
    }

    /// Wall-clock instant at which the mock most recently FINISHED sending a
    /// `result` reply matched on `subscribe_events`.  `None` if no such reply
    /// has been sent yet on this server.
    ///
    /// Used by the subscribe-ACK ordering test (TASK-046 finding 6) to assert
    /// that the client's `get_states` frame was received strictly AFTER the
    /// ACK send completed — a tighter invariant than the seq-based
    /// `snap_seq > sub_seq` check, which only proves *receipt order* of the
    /// two inbound frames at the mock.
    pub async fn subscribe_ack_sent_at(&self) -> Option<Instant> {
        self.state.lock().await.subscribe_ack_sent_at
    }

    /// Force the currently-connected client to be disconnected by sending a
    /// WebSocket `Close` frame on the next loop tick.
    ///
    /// After the close is sent the connection task exits; the flag is reset
    /// automatically.  The WsClient will reconnect per its backoff FSM, and can
    /// establish a new connection to this same mock server.  The caller must
    /// re-script the handshake replies before the reconnect arrives.
    ///
    /// Used by TASK-039's disconnect/reconnect burst scenario.
    pub fn force_disconnect(&self) {
        self.disconnect_flag.store(true, Ordering::Release);
    }
}

impl Drop for MockWsServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

async fn run_accept_loop(
    listener: TcpListener,
    state: Arc<Mutex<SharedState>>,
    seq_counter: Arc<AtomicU32>,
    disconnect_flag: Arc<AtomicBool>,
) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => return, // listener closed (mock dropped)
        };

        let conn_state = Arc::clone(&state);
        let conn_seq = Arc::clone(&seq_counter);
        let conn_flag = Arc::clone(&disconnect_flag);
        tokio::spawn(async move {
            run_connection(stream, conn_state, conn_seq, conn_flag).await;
        });
    }
}

async fn run_connection(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<SharedState>>,
    seq_counter: Arc<AtomicU32>,
    disconnect_flag: Arc<AtomicBool>,
) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let (mut write, mut read) = ws_stream.split();

    // Send all leading Immediate replies, draining them from the queue.
    loop {
        let body = {
            let mut s = state.lock().await;
            match s.replies.first() {
                Some(ScriptedReply::Immediate(_)) => {
                    if let ScriptedReply::Immediate(b) = s.replies.remove(0) {
                        b
                    } else {
                        unreachable!()
                    }
                }
                _ => break,
            }
        };
        if write.send(Message::Text(body)).await.is_err() {
            return;
        }
    }

    loop {
        // Check force-disconnect flag first.
        if disconnect_flag.swap(false, Ordering::AcqRel) {
            let _ = write.send(Message::Close(None)).await;
            return;
        }

        // Drain injected events (mid-session pushes).
        let injected = {
            let mut s = state.lock().await;
            std::mem::take(&mut s.injected)
        };
        for raw in injected {
            if write.send(Message::Text(raw)).await.is_err() {
                return;
            }
        }

        let next = tokio::time::timeout(Duration::from_millis(50), read.next()).await;
        let frame = match next {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(_))) => return,
            Ok(None) => return, // peer closed
            Err(_) => continue, // timeout — loop again to check injected/disconnect
        };

        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(b) => match String::from_utf8(b) {
                Ok(t) => t,
                Err(_) => return,
            },
            Message::Close(_) => return,
            _ => continue,
        };

        // Parse the inbound frame.
        let parsed: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = parsed
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let id = parsed.get("id").and_then(|v| v.as_u64());

        // Record the frame with a monotonically increasing seq number.
        let seq = seq_counter.fetch_add(1, Ordering::Relaxed) as u64;
        let received_at = Instant::now();
        {
            let mut s = state.lock().await;
            s.recorded.push(RecordedFrame {
                seq,
                kind: kind.clone(),
                body: text.clone(),
                received_at,
            });
        }

        // Find matching scripted reply.
        let reply_body = {
            let mut s = state.lock().await;
            let mut found: Option<String> = None;
            let mut to_remove: Option<usize> = None;
            for (i, r) in s.replies.iter().enumerate() {
                if let ScriptedReply::OnRequest {
                    match_type,
                    body,
                    forward_id,
                } = r
                {
                    if match_type == &kind {
                        let mut body = body.clone();
                        if *forward_id {
                            if let Some(id) = id {
                                body = body.replace("{{ID}}", &id.to_string());
                            }
                        }
                        found = Some(body);
                        to_remove = Some(i);
                        break;
                    }
                }
            }
            if let Some(idx) = to_remove {
                s.replies.remove(idx);
            }
            found
        };

        if let Some(body) = reply_body {
            if write.send(Message::Text(body)).await.is_err() {
                return;
            }
            // After the matched reply has been handed to tungstenite, record
            // the wall-clock send-completion time for the subscribe_events
            // ACK specifically (TASK-046 finding 6).  Other reply kinds do
            // not need this hook.  Captured AFTER the send to surface any
            // tungstenite-side queuing in the comparison.
            if kind == "subscribe_events" {
                state.lock().await.subscribe_ack_sent_at = Some(Instant::now());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

/// Build a `RawEntityState` JSON object string for `get_states` replies.
pub fn entity_state_json(
    entity_id: &str,
    state: &str,
    last_changed: &str,
    last_updated: &str,
) -> String {
    format!(
        r#"{{"entity_id":"{entity_id}","state":"{state}","attributes":{{}},"last_changed":"{last_changed}","last_updated":"{last_updated}"}}"#
    )
}

/// Build a `state_changed` event frame for `inject_event`.
///
/// `subscription_id` is the id of the `subscribe_events` request being
/// answered (HA echoes this back in every event frame).
pub fn state_changed_event_json(
    subscription_id: u32,
    entity_id: &str,
    new_state: Option<(&str, &str, &str)>,
    old_state: Option<(&str, &str, &str)>,
) -> String {
    let new_state_json = match new_state {
        Some((state, last_changed, last_updated)) => {
            entity_state_json(entity_id, state, last_changed, last_updated)
        }
        None => "null".to_owned(),
    };
    let old_state_json = match old_state {
        Some((state, last_changed, last_updated)) => {
            entity_state_json(entity_id, state, last_changed, last_updated)
        }
        None => "null".to_owned(),
    };
    format!(
        r#"{{"type":"event","id":{subscription_id},"event":{{"event_type":"state_changed","data":{{"entity_id":"{entity_id}","new_state":{new_state_json},"old_state":{old_state_json}}},"origin":"LOCAL","time_fired":"2024-01-01T00:00:00+00:00"}}}}"#
    )
}
