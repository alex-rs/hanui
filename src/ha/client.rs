//! WebSocket client for the Home Assistant real-time API.
//!
//! # FSM
//!
//! ```text
//! Connecting → Authenticating → Subscribing → Snapshotting → Services → Live
//!                                                                          │
//!                                            ←── Reconnecting ←───────────┘
//!                                                     │
//!                                                 (retry)
//!
//! auth_invalid or overflow circuit-breaker → Failed (no reconnect)
//! ```
//!
//! # Subscribe-before-snapshot sequencing gate
//!
//! The FSM does NOT issue `get_states` until it has received the `result` ACK
//! for `subscribe_events`.  This closes the race window where a state change
//! could arrive between snapshot delivery and subscription activation.
//!
//! # Snapshot buffer
//!
//! State-changed events that arrive while `get_states` is in flight are
//! buffered in a bounded ring of capacity
//! `DEFAULT_PROFILE.snapshot_buffer_events` (10 000).  After the snapshot
//! reply arrives the buffered events are replayed before the FSM transitions
//! to `Live`.  On ring overflow the connection is dropped; 3 overflows within
//! 60 s trip the circuit-breaker and transition to `Failed`.
//!
//! # Security: token handling
//!
//! The HA access token is exposed from `Config::expose_token()` exactly once,
//! at `auth` frame serialization time.  The resulting JSON bytes are written
//! directly to the WS write half.  No intermediate `String` is bound, logged,
//! or debug-printed.
//!
//! # Payload cap
//!
//! `tokio-tungstenite` is configured with `max_message_size` and
//! `max_frame_size` from `DEFAULT_PROFILE.ws_payload_cap`.  A message exceeding
//! the cap causes the transport to return an error; the FSM treats this as a
//! transport error and drops the connection for a full resync.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::sync::{oneshot, watch};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::dashboard::profiles::DEFAULT_PROFILE;
use crate::ha::protocol::{
    AuthPayload, GetServicesPayload, GetStatesPayload, InboundMsg, OutboundMsg,
    SubscribeEventsPayload,
};
use crate::platform::config::Config;
use crate::platform::status::ConnectionState;

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

/// Errors produced by the WebSocket client FSM.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The HA access token was rejected.
    ///
    /// Token plaintext is NEVER included in this error.  The `reason` field
    /// contains the human-readable message from HA with no token values.
    #[error("authentication failed: {reason}")]
    AuthInvalid { reason: String },

    /// The snapshot buffer overflowed 3 times within 60 s.
    #[error("HA instance too large for current profile")]
    OverflowCircuitBreaker,

    /// The WebSocket transport reported an error.
    #[error("WebSocket transport error: {0}")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),

    /// A `result` reply arrived with an unexpected id (correlation mismatch).
    #[error("result id {received} has no pending request")]
    IdMismatch { received: u32 },
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::Transport(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        ))
    }
}

// ---------------------------------------------------------------------------
// Internal FSM phase
// ---------------------------------------------------------------------------

/// Internal FSM phases (mirrors `ConnectionState` but with richer payload).
///
/// `pub(crate)` so that in-crate tests can construct and inspect phases
/// without exposing the type in the public API.
#[derive(Debug)]
pub(crate) enum Phase {
    /// Sending the `auth` frame; waiting for `auth_ok` or `auth_invalid`.
    Authenticating,
    /// `auth_ok` received; `subscribe_events` sent; waiting for result ACK.
    Subscribing { subscribe_id: u32 },
    /// Subscription ACK received; `get_states` in flight.
    Snapshotting {
        get_states_id: u32,
        /// Ring buffer of state-changed events arriving during snapshot fetch.
        /// Capacity: `DEFAULT_PROFILE.snapshot_buffer_events`.
        event_buffer: Vec<InboundMsg>,
    },
    /// Snapshot applied; `get_services` in flight.
    Services { get_services_id: u32 },
    /// Fully operational.
    Live,
}

// ---------------------------------------------------------------------------
// Pending-request map
// ---------------------------------------------------------------------------

/// One-shot sender waiting for a `result` reply.
type PendingSender = oneshot::Sender<Result<serde_json::Value, String>>;

// ---------------------------------------------------------------------------
// Overflow circuit-breaker
// ---------------------------------------------------------------------------

/// Tracks consecutive snapshot-buffer overflows for the circuit-breaker.
pub struct OverflowBreaker {
    /// Timestamps of recent overflow events (within 60 s window).
    pub recent: Vec<Instant>,
}

impl OverflowBreaker {
    fn new() -> Self {
        OverflowBreaker { recent: Vec::new() }
    }

    /// Record an overflow.  Returns `true` if the circuit-breaker has tripped
    /// (3 or more overflows within 60 s).
    pub fn record_overflow(&mut self) -> bool {
        let now = Instant::now();
        self.recent
            .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        self.recent.push(now);
        self.recent.len() >= 3
    }
}

// ---------------------------------------------------------------------------
// ID counter
// ---------------------------------------------------------------------------

struct IdCounter(Arc<AtomicU32>);

impl IdCounter {
    fn new() -> Self {
        IdCounter(Arc::new(AtomicU32::new(1)))
    }

    fn next(&self) -> u32 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// WsClient
// ---------------------------------------------------------------------------

/// Drives the HA WebSocket FSM.
///
/// Construct via [`WsClient::new`] and run via [`WsClient::run`].
pub struct WsClient {
    config: Config,
    state_tx: watch::Sender<ConnectionState>,
    id_counter: IdCounter,
    /// Map from request id to the oneshot sender awaiting the result.
    pending: HashMap<u32, PendingSender>,
    /// Circuit-breaker for snapshot-buffer overflows.
    pub overflow_breaker: OverflowBreaker,
}

impl WsClient {
    /// Create a new client.
    ///
    /// The `state_tx` watch sender is updated on every FSM transition so
    /// external observers (e.g. the status banner) can react.
    pub fn new(config: Config, state_tx: watch::Sender<ConnectionState>) -> Self {
        WsClient {
            config,
            state_tx,
            id_counter: IdCounter::new(),
            pending: HashMap::new(),
            overflow_breaker: OverflowBreaker::new(),
        }
    }

    /// Transition the FSM to a new `ConnectionState` and publish via watch.
    pub fn set_state(&self, state: ConnectionState) {
        tracing::info!(state = ?state, "FSM transition");
        let _ = self.state_tx.send(state);
    }

    /// Allocate a fresh message ID.
    fn next_id(&self) -> u32 {
        self.id_counter.next()
    }

    /// Register a pending request and return the receiver for its result.
    pub fn register_pending(
        &mut self,
        id: u32,
    ) -> oneshot::Receiver<Result<serde_json::Value, String>> {
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        rx
    }

    /// Resolve a pending request by id.  Returns `IdMismatch` if no pending
    /// request is registered for the id.
    // ClientError::Transport is large; this function only ever returns
    // ClientError::IdMismatch which is small, but clippy sees the enum.
    #[allow(clippy::result_large_err)]
    pub fn resolve_pending(
        &mut self,
        id: u32,
        value: Result<serde_json::Value, String>,
    ) -> Result<(), ClientError> {
        match self.pending.remove(&id) {
            Some(tx) => {
                let _ = tx.send(value);
                Ok(())
            }
            None => Err(ClientError::IdMismatch { received: id }),
        }
    }

    /// Connect to HA and run the FSM until `Failed` or a transport error.
    ///
    /// Returns `Err(ClientError::AuthInvalid)` on auth failure (no reconnect).
    /// Returns `Err(ClientError::OverflowCircuitBreaker)` when the circuit
    /// breaker trips.  Returns transport errors as `Err(ClientError::Transport)`.
    #[allow(clippy::result_large_err)]
    pub async fn run(&mut self) -> Result<(), ClientError> {
        self.set_state(ConnectionState::Connecting);
        tracing::info!(url = %self.config.url, "connecting to HA");

        let ws_config = WebSocketConfig {
            max_message_size: Some(DEFAULT_PROFILE.ws_payload_cap),
            max_frame_size: Some(DEFAULT_PROFILE.ws_payload_cap),
            ..Default::default()
        };

        // Pass the URL as a &str — tungstenite's IntoClientRequest is implemented
        // for &str and String but not url::Url.
        let (ws_stream, _response) = tokio_tungstenite::connect_async_with_config(
            self.config.url.as_str(),
            Some(ws_config),
            false,
        )
        .await
        .map_err(ClientError::Transport)?;

        tracing::info!("WebSocket connected");

        let (mut write, mut read) = ws_stream.split();

        let mut phase = Phase::Authenticating;
        self.set_state(ConnectionState::Authenticating);

        loop {
            let msg = match read.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "WebSocket transport error");
                    return Err(ClientError::Transport(e));
                }
                None => {
                    tracing::warn!("WebSocket stream closed by server");
                    return Err(ClientError::Transport(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    ));
                }
            };

            let bytes = match msg {
                Message::Text(text) => text.into_bytes(),
                Message::Binary(b) => b,
                Message::Close(_) => {
                    tracing::info!("received WS Close frame");
                    return Err(ClientError::Transport(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    ));
                }
                _ => continue,
            };

            let inbound = match serde_json::from_slice::<InboundMsg>(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse inbound message; skipping");
                    continue;
                }
            };

            phase = self.handle_message(inbound, phase, &mut write).await?;
        }
    }

    /// Handle a single inbound message, drive the FSM, return the new phase.
    ///
    /// `pub(crate)` so tests can drive the FSM directly without a live
    /// WebSocket connection.  `Phase` is also `pub(crate)`.
    ///
    /// Takes ownership of `phase` to avoid borrow conflicts when destructuring
    /// enum variants, and returns the (possibly new) phase.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn handle_message<S>(
        &mut self,
        msg: InboundMsg,
        phase: Phase,
        write: &mut S,
    ) -> Result<Phase, ClientError>
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        match (phase, msg) {
            // ── Authenticating ───────────────────────────────────────────────

            // HA sends auth_required immediately on connection.
            // SECURITY: token is exposed via expose_token() once, serialized
            // into JSON in a single expression, and written directly to the
            // sink.  The resulting String is never bound to a named variable
            // or passed to any tracing macro.
            (Phase::Authenticating, InboundMsg::AuthRequired(_)) => {
                write
                    .send(Message::Text(serde_json::to_string(&OutboundMsg::Auth(
                        AuthPayload {
                            access_token: self.config.expose_token().to_owned(),
                        },
                    ))?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!("auth frame sent");
                Ok(Phase::Authenticating)
            }

            // auth_ok → send subscribe_events, advance to Subscribing.
            (Phase::Authenticating, InboundMsg::AuthOk(_)) => {
                self.set_state(ConnectionState::Subscribing);
                let subscribe_id = self.next_id();
                write
                    .send(Message::Text(serde_json::to_string(
                        &OutboundMsg::SubscribeEvents(SubscribeEventsPayload {
                            id: subscribe_id,
                            event_type: "state_changed".to_owned(),
                        }),
                    )?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!(id = subscribe_id, "subscribe_events sent");
                Ok(Phase::Subscribing { subscribe_id })
            }

            // auth_invalid → Failed (no reconnect; token plaintext not logged).
            (Phase::Authenticating, InboundMsg::AuthInvalid(p)) => {
                tracing::error!("auth_invalid received; transitioning to Failed");
                self.set_state(ConnectionState::Failed);
                Err(ClientError::AuthInvalid { reason: p.message })
            }

            // ── Subscribing ──────────────────────────────────────────────────

            // Await the result ACK for subscribe_events.
            (Phase::Subscribing { subscribe_id }, InboundMsg::Result(result))
                if result.id == subscribe_id =>
            {
                if !result.success {
                    let reason = result
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "subscribe_events failed".to_owned());
                    tracing::error!(%reason, "subscribe_events result: failure");
                    self.set_state(ConnectionState::Failed);
                    return Err(ClientError::AuthInvalid { reason });
                }
                tracing::info!(id = subscribe_id, "subscribe_events ACK received");
                self.set_state(ConnectionState::Snapshotting);

                // Gate: ACK received — NOW issue get_states.
                let get_states_id = self.next_id();
                write
                    .send(Message::Text(serde_json::to_string(
                        &OutboundMsg::GetStates(GetStatesPayload { id: get_states_id }),
                    )?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!(id = get_states_id, "get_states sent");
                Ok(Phase::Snapshotting {
                    get_states_id,
                    event_buffer: Vec::new(),
                })
            }

            // ── Snapshotting: buffer state_changed events ────────────────────
            (
                Phase::Snapshotting {
                    get_states_id,
                    mut event_buffer,
                },
                InboundMsg::Event(event_payload),
            ) => {
                if event_buffer.len() >= DEFAULT_PROFILE.snapshot_buffer_events {
                    tracing::warn!(
                        cap = DEFAULT_PROFILE.snapshot_buffer_events,
                        "snapshot event buffer overflow; dropping connection"
                    );
                    let tripped = self.overflow_breaker.record_overflow();
                    if tripped {
                        tracing::error!("overflow circuit-breaker tripped (3 overflows in 60 s)");
                        self.set_state(ConnectionState::Failed);
                        return Err(ClientError::OverflowCircuitBreaker);
                    }
                    return Err(ClientError::Transport(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    ));
                }
                event_buffer.push(InboundMsg::Event(event_payload));
                Ok(Phase::Snapshotting {
                    get_states_id,
                    event_buffer,
                })
            }

            // Snapshot result received → replay buffered events, issue get_services.
            (
                Phase::Snapshotting {
                    get_states_id,
                    event_buffer,
                },
                InboundMsg::Result(result),
            ) if result.id == get_states_id => {
                if !result.success {
                    let reason = result
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "get_states failed".to_owned());
                    tracing::error!(%reason, "get_states result: failure");
                    self.set_state(ConnectionState::Failed);
                    return Err(ClientError::AuthInvalid { reason });
                }
                tracing::info!(
                    id = get_states_id,
                    buffered_events = event_buffer.len(),
                    "get_states snapshot received; replaying buffered events"
                );

                // Replay buffered events (TASK-030 will route to LiveStore).
                for _buffered_event in event_buffer {
                    // TASK-030: forward to LiveStore.
                }

                self.set_state(ConnectionState::Services);
                let get_services_id = self.next_id();
                write
                    .send(Message::Text(serde_json::to_string(
                        &OutboundMsg::GetServices(GetServicesPayload {
                            id: get_services_id,
                        }),
                    )?))
                    .await
                    .map_err(ClientError::Transport)?;
                tracing::info!(id = get_services_id, "get_services sent");
                Ok(Phase::Services { get_services_id })
            }

            // ── Services ────────────────────────────────────────────────────
            (Phase::Services { get_services_id }, InboundMsg::Result(result))
                if result.id == get_services_id =>
            {
                if result.success {
                    tracing::info!(id = get_services_id, "get_services reply received");
                } else {
                    tracing::warn!(
                        id = get_services_id,
                        "get_services failed; proceeding to Live anyway"
                    );
                }
                self.set_state(ConnectionState::Live);
                tracing::info!("FSM reached Live");
                Ok(Phase::Live)
            }

            // ── Live ────────────────────────────────────────────────────────

            // Mid-session auth_required → treat as transport disconnect.
            (Phase::Live, InboundMsg::AuthRequired(_)) => {
                tracing::warn!(
                    "auth_required received in Live state; \
                     treating as transport disconnect → Reconnecting"
                );
                self.set_state(ConnectionState::Reconnecting);
                Err(ClientError::Transport(
                    tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                ))
            }

            // Live events arrive here; TASK-030 will route to LiveStore.
            (Phase::Live, InboundMsg::Event(_event)) => Ok(Phase::Live),

            // ── Result correlation (any phase) ───────────────────────────────
            (phase, InboundMsg::Result(result)) => {
                let id = result.id;
                let value = if result.success {
                    Ok(result.result.unwrap_or(serde_json::Value::Null))
                } else {
                    Err(result
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "unknown error".to_owned()))
                };
                if let Err(e) = self.resolve_pending(id, value) {
                    tracing::warn!(error = %e, "unmatched result id");
                }
                Ok(phase)
            }

            // Unknown messages are silently ignored.
            (phase, InboundMsg::Unknown { type_str, .. }) => {
                tracing::debug!(type_str = %type_str, "ignoring unknown message type");
                Ok(phase)
            }

            // Any other combination: skip.
            (phase, _other) => Ok(phase),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::status;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tracing_test::traced_test;

    // -----------------------------------------------------------------------
    // In-process mock sink
    // -----------------------------------------------------------------------

    struct MockSink {
        sent: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl MockSink {
        fn new() -> (Self, Arc<tokio::sync::Mutex<Vec<String>>>) {
            let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (MockSink { sent: sent.clone() }, sent)
        }
    }

    impl futures::Sink<Message> for MockSink {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if let Message::Text(text) = item {
                // Use block_on since start_send is a synchronous trait method.
                futures::executor::block_on(async {
                    self.sent.lock().await.push(text);
                });
            }
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    // -----------------------------------------------------------------------
    // Env serialization mutex
    // -----------------------------------------------------------------------

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // -----------------------------------------------------------------------
    // Helper: build a WsClient from env
    // -----------------------------------------------------------------------

    fn make_client(token: &str) -> (WsClient, watch::Receiver<ConnectionState>) {
        unsafe {
            std::env::set_var("HA_URL", "ws://ha.local:8123/api/websocket");
            std::env::set_var("HA_TOKEN", token);
        }
        let config = Config::from_env().expect("test config");
        let (tx, rx) = status::channel();
        let client = WsClient::new(config, tx);
        (client, rx)
    }

    fn inbound(json: &str) -> InboundMsg {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("bad test JSON: {e}: {json}"))
    }

    // -----------------------------------------------------------------------
    // Test: happy path auth_ok → Live
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_ok_happy_path_reaches_live() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, state_rx) = make_client("test-token-happy");
        let (mut sink, sent) = MockSink::new();

        let messages = vec![
            inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
            inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
            // subscribe_events ACK (id=1)
            inbound(r#"{"type":"result","id":1,"success":true,"result":null}"#),
            // get_states result (id=2)
            inbound(r#"{"type":"result","id":2,"success":true,"result":[]}"#),
            // get_services result (id=3)
            inbound(r#"{"type":"result","id":3,"success":true,"result":{}}"#),
        ];

        let mut phase = Phase::Authenticating;
        for msg in messages {
            phase = client
                .handle_message(msg, phase, &mut sink)
                .await
                .expect("message should succeed");
        }

        assert!(
            matches!(phase, Phase::Live),
            "expected Live phase, got: {phase:?}"
        );
        assert_eq!(*state_rx.borrow(), ConnectionState::Live);

        let frames = sent.lock().await;
        assert_eq!(
            frames.len(),
            4,
            "expected 4 outbound frames: auth, subscribe_events, get_states, get_services"
        );

        let auth_f: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(auth_f["type"], "auth");

        let sub_f: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(sub_f["type"], "subscribe_events");
        assert_eq!(sub_f["event_type"], "state_changed");

        let gs_f: serde_json::Value = serde_json::from_str(&frames[2]).unwrap();
        assert_eq!(gs_f["type"], "get_states");

        let gsvc_f: serde_json::Value = serde_json::from_str(&frames[3]).unwrap();
        assert_eq!(gsvc_f["type"], "get_services");
    }

    // -----------------------------------------------------------------------
    // Test: auth_invalid → Failed, no reconnect
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_invalid_transitions_to_failed_no_reconnect() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, state_rx) = make_client("invalid-token");
        let (mut sink, _sent) = MockSink::new();

        // auth_required → sends auth frame, stays Authenticating.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();

        // auth_invalid → AuthInvalid error.
        let result = client
            .handle_message(
                inbound(r#"{"type":"auth_invalid","message":"Invalid access token"}"#),
                phase,
                &mut sink,
            )
            .await;

        assert!(
            matches!(result, Err(ClientError::AuthInvalid { .. })),
            "expected AuthInvalid; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Failed,
            "state must be Failed after auth_invalid"
        );

        // Token must NOT appear in the error reason.
        if let Err(ClientError::AuthInvalid { reason }) = result {
            assert!(
                !reason.contains("invalid-token"),
                "reason must not contain token; got: {reason}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test: mid-session auth_required → Reconnecting
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_mid_session_auth_required_triggers_reconnect() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, state_rx) = make_client("test-token-reconnect");
        let (mut sink, _sent) = MockSink::new();

        client.set_state(ConnectionState::Live);

        let result = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Live,
                &mut sink,
            )
            .await;

        assert!(
            matches!(
                result,
                Err(ClientError::Transport(
                    tokio_tungstenite::tungstenite::Error::ConnectionClosed
                ))
            ),
            "expected ConnectionClosed for mid-session auth_required; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Reconnecting,
            "state must be Reconnecting after mid-session auth_required"
        );
    }

    // -----------------------------------------------------------------------
    // Test: subscribe-ACK gate — get_states NOT sent until ACK received
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_states_not_sent_before_subscribe_ack() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, _state_rx) = make_client("test-token-gate");
        let (mut sink, sent) = MockSink::new();

        // Drive auth_required + auth_ok → Subscribing.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // After auth_ok: 2 frames sent (auth + subscribe_events), no get_states yet.
        {
            let frames = sent.lock().await;
            assert_eq!(frames.len(), 2);
            let sub_f: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
            assert_eq!(sub_f["type"], "subscribe_events");
            let has_get_states = frames.iter().any(|f| {
                serde_json::from_str::<serde_json::Value>(f)
                    .map(|v| v["type"] == "get_states")
                    .unwrap_or(false)
            });
            assert!(
                !has_get_states,
                "get_states must NOT be sent before subscribe_events ACK"
            );
        }

        let subscribe_id = match &phase {
            Phase::Subscribing { subscribe_id } => *subscribe_id,
            other => panic!("expected Subscribing phase; got: {other:?}"),
        };

        // Now send the subscribe ACK.
        let _phase = client
            .handle_message(
                inbound(&format!(
                    r#"{{"type":"result","id":{subscribe_id},"success":true,"result":null}}"#
                )),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // After ACK: get_states must now be present.
        {
            let frames = sent.lock().await;
            let has_get_states = frames.iter().any(|f| {
                serde_json::from_str::<serde_json::Value>(f)
                    .map(|v| v["type"] == "get_states")
                    .unwrap_or(false)
            });
            assert!(
                has_get_states,
                "get_states must be sent after subscribe_events ACK"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test: ring overflow → drop + counter increment (first overflow, no trip)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ring_overflow_triggers_drop_and_increments_counter() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, _state_rx) = make_client("test-token-overflow");
        let (mut sink, _sent) = MockSink::new();

        // Pre-fill buffer to exactly the capacity limit.
        let event_buffer: Vec<InboundMsg> = (0..DEFAULT_PROFILE.snapshot_buffer_events)
            .map(|_| {
                inbound(r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.x","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:00+00:00"}}"#)
            })
            .collect();

        let phase = Phase::Snapshotting {
            get_states_id: 42,
            event_buffer,
        };

        let extra_event = inbound(
            r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.extra","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:01+00:00"}}"#,
        );

        let result = client.handle_message(extra_event, phase, &mut sink).await;

        assert!(
            matches!(
                result,
                Err(ClientError::Transport(
                    tokio_tungstenite::tungstenite::Error::ConnectionClosed
                ))
            ),
            "first overflow must trigger a connection-drop; got: {result:?}"
        );
        assert_eq!(
            client.overflow_breaker.recent.len(),
            1,
            "overflow counter must be 1 after first overflow"
        );
    }

    // -----------------------------------------------------------------------
    // Test: 3 overflows within 60s → circuit-breaker trips → Failed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_three_overflows_trip_circuit_breaker() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, state_rx) = make_client("test-token-cb");
        let (mut sink, _sent) = MockSink::new();

        // Pre-record 2 overflows so the next one trips the breaker.
        client.overflow_breaker.record_overflow();
        client.overflow_breaker.record_overflow();

        // Trigger a 3rd overflow via handle_message.
        let event_buffer: Vec<InboundMsg> = (0..DEFAULT_PROFILE.snapshot_buffer_events)
            .map(|_| {
                inbound(r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.x","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:00+00:00"}}"#)
            })
            .collect();
        let phase = Phase::Snapshotting {
            get_states_id: 10,
            event_buffer,
        };
        let extra_event = inbound(
            r#"{"type":"event","id":1,"event":{"event_type":"state_changed","data":{"entity_id":"light.extra","new_state":null,"old_state":null},"origin":"LOCAL","time_fired":"2024-01-01T00:00:01+00:00"}}"#,
        );

        let result = client.handle_message(extra_event, phase, &mut sink).await;

        assert!(
            matches!(result, Err(ClientError::OverflowCircuitBreaker)),
            "3rd overflow must trip circuit-breaker; got: {result:?}"
        );
        assert_eq!(
            *state_rx.borrow(),
            ConnectionState::Failed,
            "state must be Failed after circuit-breaker"
        );
    }

    // -----------------------------------------------------------------------
    // Test: ID correlation — 3 concurrent requests, out-of-order replies
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_id_correlation_out_of_order_resolves_correctly() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, _state_rx) = make_client("test-token-id-correlation");

        let rx1 = client.register_pending(10);
        let rx2 = client.register_pending(20);
        let rx3 = client.register_pending(30);

        // Resolve out of order: 30, 10, 20.
        client
            .resolve_pending(30, Ok(serde_json::json!("reply-30")))
            .unwrap();
        client
            .resolve_pending(10, Ok(serde_json::json!("reply-10")))
            .unwrap();
        client
            .resolve_pending(20, Ok(serde_json::json!("reply-20")))
            .unwrap();

        assert_eq!(rx1.await.unwrap().unwrap(), serde_json::json!("reply-10"));
        assert_eq!(rx2.await.unwrap().unwrap(), serde_json::json!("reply-20"));
        assert_eq!(rx3.await.unwrap().unwrap(), serde_json::json!("reply-30"));
    }

    // -----------------------------------------------------------------------
    // Test: ID mismatch returns IdMismatch error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_id_mismatch_returns_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (mut client, _state_rx) = make_client("test-token-mismatch");

        let result = client.resolve_pending(999, Ok(serde_json::Value::Null));
        assert!(
            matches!(result, Err(ClientError::IdMismatch { received: 999 })),
            "unmatched id must produce IdMismatch; got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: token not leaked to trace after full auth handshake
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[traced_test]
    async fn test_token_not_leaked_to_trace_after_auth_handshake() {
        let _guard = ENV_LOCK.lock().unwrap();

        let plaintext_token = "UNIQUE_PLAINTEXT_TOKEN_XYZ987ABC";
        unsafe {
            std::env::set_var("HA_URL", "ws://ha.local:8123/api/websocket");
            std::env::set_var("HA_TOKEN", plaintext_token);
        }
        let config = Config::from_env().unwrap();
        let (tx, _rx) = status::channel();
        let mut client = WsClient::new(config, tx);
        let (mut sink, _sent) = MockSink::new();

        // Drive auth_required → auth_ok to exercise the token exposure path.
        let phase = client
            .handle_message(
                inbound(r#"{"type":"auth_required","ha_version":"2024.4.0"}"#),
                Phase::Authenticating,
                &mut sink,
            )
            .await
            .unwrap();
        let _phase = client
            .handle_message(
                inbound(r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#),
                phase,
                &mut sink,
            )
            .await
            .unwrap();

        // No trace event must contain the plaintext token.
        assert!(
            !logs_contain(plaintext_token),
            "plaintext token must not appear in any trace event"
        );
    }

    // -----------------------------------------------------------------------
    // Test: OverflowBreaker evicts stale events and resets
    // -----------------------------------------------------------------------

    #[test]
    fn test_overflow_breaker_evicts_old_events() {
        let mut breaker = OverflowBreaker::new();
        // Inject one stale event (>60s ago).
        breaker
            .recent
            .push(Instant::now() - Duration::from_secs(61));
        // Two more fresh overflows: after stale eviction we start from 1.
        assert!(!breaker.record_overflow()); // evicts stale; now 1
        assert!(!breaker.record_overflow()); // 2
        assert!(breaker.record_overflow()); // 3 → trip
    }

    #[test]
    fn test_overflow_breaker_does_not_trip_if_overflows_stale() {
        let mut breaker = OverflowBreaker::new();
        breaker
            .recent
            .push(Instant::now() - Duration::from_secs(62));
        breaker
            .recent
            .push(Instant::now() - Duration::from_secs(61));
        // One fresh overflow: stale events are evicted, only 1 remains → no trip.
        let tripped = breaker.record_overflow();
        assert!(!tripped);
        assert_eq!(breaker.recent.len(), 1);
    }
}
