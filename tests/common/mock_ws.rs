//! Canonical mock Home Assistant WebSocket server — shared common harness.
//!
//! This module is the **superset** of `tests/integration/mock_ws.rs`, moved to
//! `tests/common/` so that `tests/soak/` can reuse it without the soak binary
//! depending on the integration test binary.
//!
//! The sole addition vs. the integration copy is
//! [`MockWsServer::force_disconnect`], which triggers a graceful WS `Close`
//! frame on the next loop tick and is used by TASK-039's burst scenario.
//!
//! # Design
//!
//! - One [`MockWsServer`] binds to an ephemeral local port and accepts
//!   successive client connections.  Each accepted connection runs in its own
//!   Tokio task and consults the server's shared state for what to send next.
//! - A FIFO `scripted_replies` queue holds frames the server will send in
//!   response to client requests.
//! - Tests can inject events mid-session via [`MockWsServer::inject_event`]
//!   and force a connection drop via [`MockWsServer::force_disconnect`].
//!
//! # No new dependencies
//!
//! `tokio-tungstenite` is already a project dependency; `accept_async` is
//! available with the existing feature set.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Scripted reply
// ---------------------------------------------------------------------------

/// A frame the mock server will send to the client.
#[derive(Debug, Clone)]
pub enum ScriptedReply {
    /// Send this raw text frame immediately on connect.
    Immediate(String),
    /// Wait for a request whose `"type"` matches `match_type`, then send the
    /// reply.  When `forward_id` is true the request's `"id"` is substituted
    /// into the reply at the placeholder `{{ID}}`.
    OnRequest {
        match_type: String,
        body: String,
        forward_id: bool,
    },
}

// ---------------------------------------------------------------------------
// Server-side shared state
// ---------------------------------------------------------------------------

struct SharedState {
    replies: Vec<ScriptedReply>,
    injected: Vec<String>,
}

impl SharedState {
    fn new() -> Self {
        SharedState {
            replies: Vec::new(),
            injected: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// MockWsServer
// ---------------------------------------------------------------------------

/// A scriptable mock Home Assistant WebSocket server.
///
/// Construct via [`MockWsServer::start`].  Drop to shut down the accept loop.
pub struct MockWsServer {
    /// URL clients should connect to (e.g. `ws://127.0.0.1:PORT/api/websocket`).
    pub ws_url: String,
    state: Arc<Mutex<SharedState>>,
    accept_task: JoinHandle<()>,
    /// When `true` the connection loop sends a Close frame on the next tick and
    /// resets the flag.  Set via [`MockWsServer::force_disconnect`].
    disconnect_flag: Arc<AtomicBool>,
}

impl MockWsServer {
    /// Bind to an ephemeral local port and start the accept loop.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock WS: bind ephemeral port");
        let local_addr = listener.local_addr().expect("mock WS: local_addr");
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

    /// Script the `subscribe_events` ACK (`result { id, success: true }`).
    pub async fn script_subscribe_ack(&self) {
        self.push_reply(ScriptedReply::OnRequest {
            match_type: "subscribe_events".to_owned(),
            body: r#"{"type":"result","id":{{ID}},"success":true,"result":null}"#.to_owned(),
            forward_id: true,
        })
        .await;
    }

    /// Script a `get_states` reply with the given JSON entity array.
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
    /// loop tick.
    pub async fn inject_event(&self, raw: String) {
        self.state.lock().await.injected.push(raw);
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
            Err(_) => return,
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

    // Drain and send all leading Immediate replies.
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

        // Drain injected events.
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
            Ok(None) => return,
            Err(_) => continue,
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

        // Parse, record seq (discarded here — soak test doesn't need ordering
        // assertions), and find a matching scripted reply.
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
        // Advance the counter even though no caller reads it from this module,
        // so the counter remains monotonically consistent if callers are added.
        let _ = seq_counter.fetch_add(1, Ordering::Relaxed);

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
/// `subscription_id` is the id of the `subscribe_events` request.
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
