//! Canonical mock Home Assistant WebSocket server for Phase 2 integration tests.
//!
//! This is the **canonical** mock harness used by TASK-035 and reused by
//! TASK-038 (churn benchmark), TASK-039 (memory soak), and TASK-040 (SBC CPU
//! smoke).  A second mock WS implementation is forbidden; tests must drive
//! `MockWsServer` rather than rolling their own.
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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
/// order across two different message kinds.
#[derive(Debug, Clone)]
pub struct RecordedFrame {
    pub seq: u64,
    pub kind: String,
    pub body: String,
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
    /// reply.  If `forward_id` is true the request's `"id"` is substituted into
    /// the reply at the literal placeholder `{{ID}}` (HA result-correlation
    /// pattern).
    OnRequest {
        match_type: String,
        body: String,
        forward_id: bool,
    },
    /// Drop the connection cleanly when a request matching `match_type` arrives.
    DisconnectOn { match_type: String },
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
    /// If true, the next connection's accept loop will drop the connection
    /// without sending any further frames after processing the next request.
    force_disconnect: bool,
}

impl SharedState {
    fn new() -> Self {
        SharedState {
            replies: Vec::new(),
            recorded: Vec::new(),
            injected: Vec::new(),
            force_disconnect: false,
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
    seq_counter: Arc<AtomicU32>,
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

        let accept_state = Arc::clone(&state);
        let accept_seq = Arc::clone(&seq_counter);
        let accept_task = tokio::spawn(async move {
            run_accept_loop(listener, accept_state, accept_seq).await;
        });

        MockWsServer {
            ws_url,
            state,
            accept_task,
            seq_counter,
        }
    }

    /// Append a scripted reply to the FIFO queue.
    ///
    /// Public helpers (`script_auth_ok`, `script_subscribe_ack`, etc.) wrap this
    /// for the common HA flows.
    pub async fn push_reply(&self, reply: ScriptedReply) {
        self.state.lock().await.replies.push(reply);
    }

    /// Script `auth_required` (immediate) followed by `auth_ok` (on receipt of
    /// `auth`).  This is the standard HA hand-shake.
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

    /// Script the `subscribe_events` ACK (`result { id, success: true }`).
    pub async fn script_subscribe_ack(&self) {
        self.push_reply(ScriptedReply::OnRequest {
            match_type: "subscribe_events".to_owned(),
            body: r#"{"type":"result","id":{{ID}},"success":true,"result":null}"#.to_owned(),
            forward_id: true,
        })
        .await;
    }

    /// Script the `get_states` reply with the given JSON entity array (raw).
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

    /// Script the `get_services` reply with the given JSON service map.
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

    /// Inject a mid-session `auth_required` re-prompt.
    pub async fn inject_auth_required(&self) {
        self.inject_event(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#.to_owned())
            .await;
    }

    /// Force the next active connection to drop after processing the next
    /// inbound request.  Tests use this to verify reconnect behaviour.
    pub async fn force_disconnect(&self) {
        self.state.lock().await.force_disconnect = true;
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

    /// Wait until at least `n` frames matching `kind` have been recorded, or
    /// the timeout elapses.  Returns true if the count was reached.
    pub async fn wait_for_request(&self, kind: &str, n: usize, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        loop {
            if self.recorded_request_count(kind).await >= n {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Snapshot all recorded frames (for debugging assertions).
    #[allow(dead_code)]
    pub async fn dump_recorded(&self) -> String {
        let recorded = self.recorded_requests().await;
        recorded
            .iter()
            .map(|r| format!("[{}] {} {}", r.seq, r.kind, r.body))
            .collect::<Vec<_>>()
            .join("\n")
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
) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => return, // listener closed (mock dropped)
        };

        let conn_state = Arc::clone(&state);
        let conn_seq = Arc::clone(&seq_counter);
        tokio::spawn(async move {
            run_connection(stream, conn_state, conn_seq).await;
        });
    }
}

async fn run_connection(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<SharedState>>,
    seq_counter: Arc<AtomicU32>,
) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let (mut write, mut read) = ws_stream.split();

    // Send all leading Immediate replies, draining them from the queue.
    {
        let mut s = state.lock().await;
        let mut idx = 0;
        while idx < s.replies.len() {
            if let ScriptedReply::Immediate(body) = &s.replies[idx] {
                let body = body.clone();
                drop(s);
                if write.send(Message::Text(body)).await.is_err() {
                    return;
                }
                s = state.lock().await;
                s.replies.remove(idx);
                continue;
            }
            break;
        }
    }

    loop {
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

        // Record the frame.
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
        let seq = seq_counter.fetch_add(1, Ordering::Relaxed) as u64;
        {
            let mut s = state.lock().await;
            s.recorded.push(RecordedFrame {
                seq,
                kind: kind.clone(),
                body: text.clone(),
            });
        }

        // Find matching scripted reply.
        let reply_body = {
            let mut s = state.lock().await;
            let mut found: Option<String> = None;
            let mut to_remove: Option<usize> = None;
            for (i, r) in s.replies.iter().enumerate() {
                match r {
                    ScriptedReply::OnRequest {
                        match_type,
                        body,
                        forward_id,
                    } if match_type == &kind => {
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
                    ScriptedReply::DisconnectOn { match_type } if match_type == &kind => {
                        to_remove = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(idx) = to_remove {
                let removed = s.replies.remove(idx);
                if matches!(removed, ScriptedReply::DisconnectOn { .. }) {
                    return;
                }
            }
            found
        };

        if let Some(body) = reply_body {
            if write.send(Message::Text(body)).await.is_err() {
                return;
            }
        }

        // Honor force_disconnect after handling the request.
        let should_disconnect = {
            let mut s = state.lock().await;
            let f = s.force_disconnect;
            if f {
                s.force_disconnect = false;
            }
            f
        };
        if should_disconnect {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: build a fixture entity-state JSON suitable for `script_get_states_reply`.
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
