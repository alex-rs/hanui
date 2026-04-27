//! Typed Home Assistant WebSocket protocol message enums.
//!
//! All outbound messages are serialized to JSON and sent over the WS
//! connection; all inbound messages are deserialized from JSON.
//!
//! The outer [`InboundMsg`] enum uses `#[serde(tag = "type")]` internally
//! with a custom deserializer that converts unknown `type` values to
//! [`InboundMsg::Unknown`] rather than panicking or returning an error.  This
//! tolerates HA adding new message types without breaking the client.
//!
//! # HA version range
//!
//! Message variants are pinned to the HA WebSocket API as documented at
//! <https://developers.home-assistant.io/docs/api/websocket/> (tested against
//! HA 2024.x).  If HA adds required fields in a future release, the typed
//! deserialize paths will surface it via [`ParseError::Json`] rather than
//! silently dropping data, allowing callers to handle the mismatch explicitly.
//!
//! # Payload size constraint
//!
//! The WS transport layer (`src/ha/client.rs`, implemented in TASK-029)
//! enforces the maximum message size via `tokio-tungstenite`
//! `max_message_size` / `max_frame_size`, both set to
//! `DEFAULT_PROFILE.ws_payload_cap` (16 MiB for the desktop profile).  Any
//! message exceeding that cap is dropped at the transport level; the client
//! then performs a full resync.  This module documents that constraint via the
//! [`ParseError::OversizedFrame`] variant, which the transport layer emits
//! before attempting deserialization so callers have a single typed error
//! hierarchy to match against.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::dashboard::profiles::DEFAULT_PROFILE;

// ---------------------------------------------------------------------------
// ParseError
// ---------------------------------------------------------------------------

/// Errors that can occur when parsing an inbound HA WebSocket frame.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The JSON payload could not be deserialized into any known message shape.
    ///
    /// This is a hard protocol error — the frame was valid UTF-8 and parseable
    /// as JSON but the structure did not match any known HA message schema.
    #[error("JSON deserialize error: {0}")]
    Json(#[from] serde_json::Error),

    /// The frame size exceeded the configured cap before deserialization was
    /// attempted.
    ///
    /// Cap is sourced from `DEFAULT_PROFILE.ws_payload_cap` (currently
    /// 16 MiB for the desktop profile).  Enforcement happens in the transport
    /// layer (`src/ha/client.rs`); this variant is emitted so callers have a
    /// single typed error hierarchy to match against.
    ///
    /// On receiving this error, the caller must drop the connection and
    /// initiate a full resync — no partial buffer may be retained.
    #[error("frame size {actual_bytes} exceeds cap {cap_bytes} (DEFAULT_PROFILE.ws_payload_cap)")]
    OversizedFrame {
        actual_bytes: usize,
        cap_bytes: usize,
    },
}

impl ParseError {
    /// Construct an [`OversizedFrame`][ParseError::OversizedFrame] using the
    /// cap from [`DEFAULT_PROFILE`].
    pub fn oversized(actual_bytes: usize) -> Self {
        ParseError::OversizedFrame {
            actual_bytes,
            cap_bytes: DEFAULT_PROFILE.ws_payload_cap,
        }
    }
}

// ---------------------------------------------------------------------------
// HaError — inner error object returned inside `result` frames
// ---------------------------------------------------------------------------

/// An error object embedded in a HA `result` frame when `success` is false.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HaError {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Outbound messages
// ---------------------------------------------------------------------------

/// Outbound WS messages sent from the client to HA.
///
/// Each variant serializes with a `"type"` field via `#[serde(tag = "type")]`.
///
/// # Debug redaction
///
/// [`OutboundMsg::Auth`] contains the `access_token` field.  The default
/// `derive(Debug)` would print the token in plain text, which violates the
/// security rule "never log secrets".  Therefore `Debug` is implemented
/// manually on [`AuthPayload`] to redact the token.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundMsg {
    /// Authenticate with a long-lived access token.
    ///
    /// Sent immediately after receiving `auth_required`.  The `access_token`
    /// field is plain `String` at the protocol-types level; the bridge from
    /// `SecretString` happens in TASK-029 via `Config::expose_token()`.
    Auth(AuthPayload),
    /// Subscribe to events of a given type.
    SubscribeEvents(SubscribeEventsPayload),
    /// Request the current state snapshot for all entities.
    GetStates(GetStatesPayload),
    /// Request the current service registry.
    GetServices(GetServicesPayload),
}

impl std::fmt::Debug for OutboundMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutboundMsg::Auth(_) => f
                .debug_struct("OutboundMsg::Auth")
                .field("access_token", &"[REDACTED]")
                .finish(),
            OutboundMsg::SubscribeEvents(p) => f
                .debug_struct("OutboundMsg::SubscribeEvents")
                .field("id", &p.id)
                .field("event_type", &p.event_type)
                .finish(),
            OutboundMsg::GetStates(p) => f
                .debug_struct("OutboundMsg::GetStates")
                .field("id", &p.id)
                .finish(),
            OutboundMsg::GetServices(p) => f
                .debug_struct("OutboundMsg::GetServices")
                .field("id", &p.id)
                .finish(),
        }
    }
}

/// Payload for the `auth` outbound frame.
///
/// # Security: never Debug-print or log this struct
///
/// The `access_token` field carries the raw long-lived HA access token.
/// It MUST NOT be passed to any `tracing` macro, formatted with `{:?}`,
/// printed with `println!`, or stored in any intermediate `String` binding
/// that outlives a single statement.  The token is exposed from
/// `SecretString` exactly once, at the moment of WS frame construction
/// (see TASK-029 `src/ha/client.rs`), and the resulting JSON is written
/// directly to the WS write half — never logged or printed.
#[derive(Serialize)]
pub struct AuthPayload {
    /// Raw access token — see the security doc-comment on this struct.
    pub access_token: String,
}

/// Payload for the `subscribe_events` outbound frame.
///
/// `event_type` is `Option<String>` because HA's WS API documents the field as
/// optional — omitting it subscribes to ALL events on the bus
/// (<https://developers.home-assistant.io/docs/api/websocket/#subscribe-to-events>).
/// TASK-049 changed the client from a `state_changed`-only filter to subscribe-all
/// so `service_registered` / `service_removed` events flow on the same single
/// subscription, with internal dispatch by [`EventVariant`].  The single-ACK
/// gate is preserved (one outbound frame, one ACK to wait for) and so are
/// the existing snapshot-buffer and ordering invariants.
///
/// `#[serde(skip_serializing_if = "Option::is_none")]` ensures the field is
/// omitted from the serialized JSON when `None`, matching HA's expectation
/// that absence — not `null` — means "all events".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeEventsPayload {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
}

/// Payload for the `get_states` outbound frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetStatesPayload {
    pub id: u32,
}

/// Payload for the `get_services` outbound frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetServicesPayload {
    pub id: u32,
}

// ---------------------------------------------------------------------------
// Inbound messages
// ---------------------------------------------------------------------------

/// An inbound WS message from HA, deserialized from the raw JSON frame.
///
/// Deserialization uses a two-step approach: first parse to an untyped
/// `serde_json::Value`, then dispatch on the `"type"` field.  This gives us
/// control over unknown `type` values — they map to [`InboundMsg::Unknown`]
/// instead of returning an error, so the client tolerates HA adding new
/// message types without crashing.
///
/// # Note on `#[serde(tag = "type")]`
///
/// A plain `#[serde(tag = "type")]` derive would panic (return an error) on an
/// unknown variant.  Instead, this enum is deserialized via a custom
/// [`Deserialize`] impl that delegates to [`RawInboundMsg`] and maps
/// [`RawInboundMsg::Unknown`] to [`InboundMsg::Unknown`].
#[derive(Debug, Clone)]
pub enum InboundMsg {
    /// HA requires authentication before accepting any commands.
    AuthRequired(AuthRequiredPayload),
    /// Authentication succeeded.
    AuthOk(AuthOkPayload),
    /// Authentication failed — the access token is invalid or revoked.
    ///
    /// On receiving this variant, the client MUST transition to `Failed` and
    /// MUST NOT attempt to reconnect automatically (the token is permanently
    /// invalid until the user rotates it).
    AuthInvalid(AuthInvalidPayload),
    /// An event notification from HA.
    Event(Box<EventPayload>),
    /// A response to a command previously sent with a numeric `id`.
    Result(ResultPayload),
    /// A message with an unrecognized `"type"` field.
    ///
    /// The raw type string is preserved for diagnostics; the raw JSON value is
    /// retained so callers can forward-parse new message types if needed.
    Unknown { type_str: String, raw: Value },
}

// Intermediate representation used during deserialization.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawInboundMsg {
    AuthRequired(AuthRequiredPayload),
    AuthOk(AuthOkPayload),
    AuthInvalid(AuthInvalidPayload),
    Event(Box<EventPayload>),
    Result(ResultPayload),
    #[serde(other)]
    Unknown,
}

impl<'de> Deserialize<'de> for InboundMsg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deserialize to an untyped Value first so we can recover the `type`
        // field and the full raw object for the Unknown variant.
        let raw_value = Value::deserialize(deserializer)?;

        // Try to parse as a known variant.  `serde_json::from_value` clones
        // the Value; this is acceptable because the parse path is not hot.
        match serde_json::from_value::<RawInboundMsg>(raw_value.clone()) {
            Ok(RawInboundMsg::AuthRequired(p)) => Ok(InboundMsg::AuthRequired(p)),
            Ok(RawInboundMsg::AuthOk(p)) => Ok(InboundMsg::AuthOk(p)),
            Ok(RawInboundMsg::AuthInvalid(p)) => Ok(InboundMsg::AuthInvalid(p)),
            Ok(RawInboundMsg::Event(p)) => Ok(InboundMsg::Event(p)),
            Ok(RawInboundMsg::Result(p)) => Ok(InboundMsg::Result(p)),
            Ok(RawInboundMsg::Unknown) => {
                let type_str = raw_value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<missing>")
                    .to_owned();
                Ok(InboundMsg::Unknown {
                    type_str,
                    raw: raw_value,
                })
            }
            Err(e) => Err(serde::de::Error::custom(e)),
        }
    }
}

/// Parse a raw JSON byte slice into an [`InboundMsg`].
///
/// Returns [`ParseError::OversizedFrame`] before attempting deserialization if
/// the slice exceeds `DEFAULT_PROFILE.ws_payload_cap`.  The caller should
/// check this case first to avoid allocating for oversized frames.
///
/// Returns [`ParseError::Json`] for malformed JSON or unknown schema.
pub fn parse_inbound(bytes: &[u8]) -> Result<InboundMsg, ParseError> {
    if bytes.len() > DEFAULT_PROFILE.ws_payload_cap {
        return Err(ParseError::oversized(bytes.len()));
    }
    let msg = serde_json::from_slice(bytes)?;
    Ok(msg)
}

// ---------------------------------------------------------------------------
// Inbound payload types
// ---------------------------------------------------------------------------

/// Payload for `auth_required` — sent by HA immediately on WS connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthRequiredPayload {
    pub ha_version: String,
}

/// Payload for `auth_ok` — sent when authentication succeeds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthOkPayload {
    pub ha_version: String,
}

/// Payload for `auth_invalid` — sent when the token is rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthInvalidPayload {
    pub message: String,
}

/// Payload for an `event` frame from HA.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPayload {
    /// The correlation ID matching the `subscribe_events` request.
    pub id: u32,
    pub event: EventVariant,
}

/// The discriminated union of event types arriving inside an `event` frame.
///
/// HA does not include a top-level discriminator field on the event object;
/// the discriminator is the inner `event_type` string.  The previous
/// implementation used `#[serde(untagged)]` and relied on serde's
/// shape-matching to pick a variant — that worked when only `StateChanged`
/// existed, but `ServiceRegistered` and `ServiceRemoved` (TASK-049) share an
/// identical `{event_type, data:{domain,service}, origin, time_fired}` shape,
/// so untagged could not distinguish them.
///
/// A custom [`Deserialize`] impl peeks at `event_type` and dispatches to the
/// matching variant by value, falling back to [`EventVariant::Other`] for
/// any unknown event type so the FSM continues to tolerate HA emitting new
/// event types without crashing.
///
/// The `Serialize` impl is still derived; round-tripping a typed variant
/// produces the exact JSON shape HA emitted.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum EventVariant {
    /// A `state_changed` event.
    StateChanged(Box<StateChangedEvent>),
    /// A `service_registered` event — HA fires this when a new service action
    /// is registered with a domain (e.g., a freshly-loaded integration).
    ///
    /// The event payload at the WS level is documented as
    /// `{event_type:"service_registered", data:{domain, service}, origin,
    /// time_fired, ...}`.  HA does not include the service's metadata schema
    /// in the event, so the client must default the [`crate::ha::services::ServiceMeta`]
    /// fields to their `Default` values until the next `get_services` round.
    /// See `src/ha/client.rs`'s `Phase::Live` event-dispatch arm for the
    /// register-time apply.
    ServiceRegistered(Box<ServiceLifecycleEvent>),
    /// A `service_removed` event — HA fires this when a service action is
    /// removed (e.g., an integration is unloaded).
    ///
    /// Payload is structurally identical to [`EventVariant::ServiceRegistered`]:
    /// `{event_type:"service_removed", data:{domain, service}, ...}`.
    ServiceRemoved(Box<ServiceLifecycleEvent>),
    /// Any event whose `event_type` does not match a known typed variant.
    ///
    /// The full raw JSON is preserved so future variants can be added without
    /// silently discarding data.
    Other(Value),
}

impl<'de> Deserialize<'de> for EventVariant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Two-step: deserialize to Value, peek `event_type`, dispatch.  Mirrors
        // the existing `InboundMsg` Deserialize pattern at the top of this file.
        // The clone of `raw_value` for the typed `from_value` call is acceptable
        // because the parse path is not hot — events arrive at human-scale
        // frequencies, not tight-loop frequencies.
        let raw_value = Value::deserialize(deserializer)?;

        let event_type = raw_value
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            "state_changed" => serde_json::from_value::<StateChangedEvent>(raw_value)
                .map(|sc| EventVariant::StateChanged(Box::new(sc)))
                .map_err(serde::de::Error::custom),
            "service_registered" => serde_json::from_value::<ServiceLifecycleEvent>(raw_value)
                .map(|sl| EventVariant::ServiceRegistered(Box::new(sl)))
                .map_err(serde::de::Error::custom),
            "service_removed" => serde_json::from_value::<ServiceLifecycleEvent>(raw_value)
                .map(|sl| EventVariant::ServiceRemoved(Box::new(sl)))
                .map_err(serde::de::Error::custom),
            _ => Ok(EventVariant::Other(raw_value)),
        }
    }
}

/// A `state_changed` event nested inside an `event` frame.
///
/// The `event_type` field must equal `"state_changed"`.  Callers that need to
/// filter on the type string should inspect [`StateChangedEvent::event_type`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateChangedEvent {
    pub event_type: String,
    pub data: StateChangedData,
    pub origin: String,
    pub time_fired: String,
}

/// The `data` block inside a `state_changed` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateChangedData {
    pub entity_id: String,
    /// The new state; `None` if the entity was removed.
    pub new_state: Option<RawEntityState>,
    /// The old state; `None` for a newly-created entity.
    pub old_state: Option<RawEntityState>,
}

/// A `service_registered` or `service_removed` event nested inside an `event`
/// frame.
///
/// Both event types share the exact same payload structure per HA's
/// documentation
/// (<https://www.home-assistant.io/docs/configuration/events/>):
/// `{event_type, data:{domain, service}, origin, time_fired}`.  The variant
/// in [`EventVariant`] is the discriminator the client uses to decide between
/// `add_service` and `remove_service`; the `event_type` field on this struct
/// is preserved for diagnostics and for round-trip serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceLifecycleEvent {
    pub event_type: String,
    pub data: ServiceLifecycleData,
    pub origin: String,
    pub time_fired: String,
}

/// The `data` block inside a `service_registered` or `service_removed` event.
///
/// HA's documented schema lists exactly two fields — `domain` and `service`.
/// No `ServiceMeta` is included; on `service_registered` the client MUST
/// populate the registry with a default-empty [`crate::ha::services::ServiceMeta`]
/// because the event does not carry the field schema.  The next successful
/// `get_services` round refills it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceLifecycleData {
    pub domain: String,
    pub service: String,
}

/// A raw entity state as received from HA in event or snapshot frames.
///
/// This is intentionally kept as a minimal typed struct rather than a full
/// `Entity` to avoid coupling the protocol layer to the store layer.
/// Conversion to [`crate::ha::entity::Entity`] happens in the client/store
/// layer (TASK-029/TASK-030).
///
/// # Debug redaction
///
/// The `attributes` field is omitted from [`Debug`] output because it is an
/// arbitrary JSON map that may contain user-supplied values.  Per the security
/// rule "never log secrets or full request/response bodies", the attributes
/// map must not appear in trace output.  The `entity_id` and `state` fields
/// are safe diagnostic identifiers and are included.
#[derive(Clone, Serialize, Deserialize)]
pub struct RawEntityState {
    pub entity_id: String,
    pub state: String,
    pub attributes: Value,
    pub last_changed: String,
    pub last_updated: String,
}

impl std::fmt::Debug for RawEntityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawEntityState")
            .field("entity_id", &self.entity_id)
            .field("state", &self.state)
            .field("attributes", &"[REDACTED]")
            .field("last_changed", &self.last_changed)
            .field("last_updated", &self.last_updated)
            .finish()
    }
}

/// Payload for a `result` frame (command acknowledgement).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultPayload {
    pub id: u32,
    pub success: bool,
    pub error: Option<HaError>,
    /// The result data for commands that return data (e.g. `get_states`).
    ///
    /// `None` for ACK-only results (e.g. subscription confirmation).
    #[serde(default)]
    pub result: Option<Value>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Outbound: Auth debug redaction
    // -----------------------------------------------------------------------

    #[test]
    fn auth_debug_output_redacts_token() {
        let msg = OutboundMsg::Auth(AuthPayload {
            access_token: "super_secret_token_abc123".to_owned(),
        });
        let debug_str = format!("{msg:?}");
        assert!(
            !debug_str.contains("super_secret_token_abc123"),
            "access_token must not appear in Debug output; got: {debug_str}"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]; got: {debug_str}"
        );
    }

    // -----------------------------------------------------------------------
    // Outbound: round-trip serialization for all variants
    // -----------------------------------------------------------------------

    #[test]
    fn outbound_auth_serializes_correctly() {
        let msg = OutboundMsg::Auth(AuthPayload {
            access_token: "test_token".to_owned(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "auth");
        assert_eq!(v["access_token"], "test_token");
    }

    #[test]
    fn outbound_subscribe_events_with_specific_type_round_trip() {
        // Backwards-compat: a `Some(event_type)` still serializes the field
        // (used by callers that want to filter to a single event type).
        let msg = OutboundMsg::SubscribeEvents(SubscribeEventsPayload {
            id: 1,
            event_type: Some("state_changed".to_owned()),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "subscribe_events");
        assert_eq!(v["id"], 1);
        assert_eq!(v["event_type"], "state_changed");
    }

    #[test]
    fn outbound_subscribe_events_with_none_omits_event_type_for_all_events() {
        // TASK-049: the client uses `event_type: None` to subscribe to ALL
        // events (so `service_registered` / `service_removed` flow on the
        // same single subscription).  HA's WS API requires the field to be
        // ABSENT — not `null` — to mean "all events".  This test pins the
        // serialization shape against accidental regression to a `null` value
        // (which `#[serde(default)]` on the `Option` would otherwise allow).
        let msg = OutboundMsg::SubscribeEvents(SubscribeEventsPayload {
            id: 7,
            event_type: None,
        });
        let json = serde_json::to_string(&msg).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "subscribe_events");
        assert_eq!(v["id"], 7);
        assert!(
            v.get("event_type").is_none(),
            "event_type must be ABSENT (not null) when None; got JSON: {json}"
        );
        assert!(
            !json.contains("event_type"),
            "serialized JSON must not contain the event_type key at all when None; \
             got: {json}"
        );
    }

    #[test]
    fn outbound_get_states_round_trip() {
        let msg = OutboundMsg::GetStates(GetStatesPayload { id: 2 });
        let json = serde_json::to_string(&msg).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "get_states");
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn outbound_get_services_round_trip() {
        let msg = OutboundMsg::GetServices(GetServicesPayload { id: 3 });
        let json = serde_json::to_string(&msg).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "get_services");
        assert_eq!(v["id"], 3);
    }

    // -----------------------------------------------------------------------
    // Inbound: auth_required
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_auth_required_deserializes() {
        let json = r#"{"type":"auth_required","ha_version":"2024.4.0"}"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::AuthRequired(p) => {
                assert_eq!(p.ha_version, "2024.4.0");
            }
            other => panic!("expected AuthRequired, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound: auth_ok
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_auth_ok_deserializes() {
        let json = r#"{"type":"auth_ok","ha_version":"2024.4.0"}"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::AuthOk(p) => {
                assert_eq!(p.ha_version, "2024.4.0");
            }
            other => panic!("expected AuthOk, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound: auth_invalid
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_auth_invalid_deserializes() {
        let json = r#"{"type":"auth_invalid","message":"Invalid access token or password"}"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::AuthInvalid(p) => {
                assert_eq!(p.message, "Invalid access token or password");
            }
            other => panic!("expected AuthInvalid, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound: result (ACK)
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_result_success_deserializes() {
        let json = r#"{"type":"result","id":1,"success":true,"result":null}"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Result(p) => {
                assert_eq!(p.id, 1);
                assert!(p.success);
                assert!(p.error.is_none());
            }
            other => panic!("expected Result, got: {other:?}"),
        }
    }

    #[test]
    fn inbound_result_failure_with_error_deserializes() {
        let json = r#"{
            "type": "result",
            "id": 5,
            "success": false,
            "error": {
                "code": "not_found",
                "message": "Entity light.missing not found"
            }
        }"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Result(p) => {
                assert_eq!(p.id, 5);
                assert!(!p.success);
                let err = p.error.unwrap();
                assert_eq!(err.code, "not_found");
                assert_eq!(err.message, "Entity light.missing not found");
            }
            other => panic!("expected Result, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound: event { state_changed }
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_event_state_changed_deserializes() {
        let json = r#"{
            "type": "event",
            "id": 1,
            "event": {
                "event_type": "state_changed",
                "data": {
                    "entity_id": "light.kitchen",
                    "new_state": {
                        "entity_id": "light.kitchen",
                        "state": "on",
                        "attributes": {"brightness": 180, "friendly_name": "Kitchen"},
                        "last_changed": "2024-04-01T12:00:00.000000+00:00",
                        "last_updated": "2024-04-01T12:00:00.000000+00:00"
                    },
                    "old_state": {
                        "entity_id": "light.kitchen",
                        "state": "off",
                        "attributes": {"brightness": 0, "friendly_name": "Kitchen"},
                        "last_changed": "2024-04-01T11:00:00.000000+00:00",
                        "last_updated": "2024-04-01T11:00:00.000000+00:00"
                    }
                },
                "origin": "LOCAL",
                "time_fired": "2024-04-01T12:00:00.000000+00:00"
            }
        }"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Event(p) => {
                assert_eq!(p.id, 1);
                match &p.event {
                    EventVariant::StateChanged(sc) => {
                        assert_eq!(sc.event_type, "state_changed");
                        assert_eq!(sc.data.entity_id, "light.kitchen");
                        let new_state = sc.data.new_state.as_ref().unwrap();
                        assert_eq!(new_state.state, "on");
                        let old_state = sc.data.old_state.as_ref().unwrap();
                        assert_eq!(old_state.state, "off");
                    }
                    other => panic!("expected StateChanged event, got: {other:?}"),
                }
            }
            other => panic!("expected Event, got: {other:?}"),
        }
    }

    #[test]
    fn inbound_event_with_null_new_state_signals_removal() {
        let json = r#"{
            "type": "event",
            "id": 1,
            "event": {
                "event_type": "state_changed",
                "data": {
                    "entity_id": "light.removed",
                    "new_state": null,
                    "old_state": {
                        "entity_id": "light.removed",
                        "state": "on",
                        "attributes": {},
                        "last_changed": "2024-04-01T12:00:00.000000+00:00",
                        "last_updated": "2024-04-01T12:00:00.000000+00:00"
                    }
                },
                "origin": "LOCAL",
                "time_fired": "2024-04-01T12:00:01.000000+00:00"
            }
        }"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Event(p) => match &p.event {
                EventVariant::StateChanged(sc) => {
                    assert!(
                        sc.data.new_state.is_none(),
                        "null new_state must be None (signals entity removal)"
                    );
                }
                other => panic!("expected StateChanged, got: {other:?}"),
            },
            other => panic!("expected Event, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound: event { service_registered }
    // -----------------------------------------------------------------------

    #[test]
    fn inbound_event_service_registered_deserializes() {
        // TASK-049: HA emits `service_registered` events on the WS bus when an
        // integration registers a new service action.  The client must parse
        // these into a typed variant so the FSM can apply them to the shared
        // `ServiceRegistry` rather than drop them as `Other(Value)`.
        let json = r#"{
            "type": "event",
            "id": 1,
            "event": {
                "event_type": "service_registered",
                "data": {
                    "domain": "light",
                    "service": "toggle"
                },
                "origin": "LOCAL",
                "time_fired": "2024-04-01T12:00:00.000000+00:00"
            }
        }"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Event(p) => match &p.event {
                EventVariant::ServiceRegistered(sr) => {
                    assert_eq!(sr.event_type, "service_registered");
                    assert_eq!(sr.data.domain, "light");
                    assert_eq!(sr.data.service, "toggle");
                    assert_eq!(sr.origin, "LOCAL");
                }
                other => panic!("expected ServiceRegistered, got: {other:?}"),
            },
            other => panic!("expected Event, got: {other:?}"),
        }
    }

    #[test]
    fn inbound_event_service_removed_deserializes_distinctly_from_registered() {
        // Co-assertion proves the custom Deserialize uses `event_type` as the
        // discriminator (not just shape-matching, which would conflate
        // service_registered and service_removed since their data shapes are
        // identical).
        let json = r#"{
            "type": "event",
            "id": 1,
            "event": {
                "event_type": "service_removed",
                "data": {
                    "domain": "switch",
                    "service": "deprecated_action"
                },
                "origin": "LOCAL",
                "time_fired": "2024-04-01T12:00:01.000000+00:00"
            }
        }"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Event(p) => match &p.event {
                EventVariant::ServiceRemoved(sr) => {
                    assert_eq!(sr.event_type, "service_removed");
                    assert_eq!(sr.data.domain, "switch");
                    assert_eq!(sr.data.service, "deprecated_action");
                }
                EventVariant::ServiceRegistered(_) => {
                    panic!(
                        "service_removed must NOT deserialize as ServiceRegistered \
                         — the event_type field is the discriminator"
                    );
                }
                other => panic!("expected ServiceRemoved, got: {other:?}"),
            },
            other => panic!("expected Event, got: {other:?}"),
        }
    }

    #[test]
    fn inbound_event_unknown_event_type_falls_through_to_other() {
        // A future HA event type the client doesn't yet recognise must not
        // crash deserialization — it falls through to EventVariant::Other and
        // the FSM ignores it.  Pins the resilience profile.
        let json = r#"{
            "type": "event",
            "id": 1,
            "event": {
                "event_type": "future_event_from_ha_next_version",
                "data": {"any": "shape"},
                "origin": "LOCAL",
                "time_fired": "2024-04-01T12:00:00.000000+00:00"
            }
        }"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Event(p) => match &p.event {
                EventVariant::Other(v) => {
                    assert_eq!(v["event_type"], "future_event_from_ha_next_version");
                }
                other => panic!("expected Other, got: {other:?}"),
            },
            other => panic!("expected Event, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound: unknown type — MUST NOT panic
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_type_deserializes_without_panic() {
        let json = r#"{"type":"hello_unknown_msg","some_field":"some_value"}"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Unknown { type_str, .. } => {
                assert_eq!(type_str, "hello_unknown_msg");
            }
            other => panic!("expected Unknown, got: {other:?}"),
        }
    }

    #[test]
    fn unknown_type_preserves_raw_value() {
        let json = r#"{"type":"pong","context":{"id":"ABC"}}"#;
        let msg = serde_json::from_str::<InboundMsg>(json).unwrap();
        match msg {
            InboundMsg::Unknown { type_str, raw } => {
                assert_eq!(type_str, "pong");
                assert_eq!(raw["context"]["id"], "ABC");
            }
            other => panic!("expected Unknown, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound: malformed JSON returns ParseError::Json, not panic
    // -----------------------------------------------------------------------

    #[test]
    fn malformed_json_returns_typed_error() {
        let bad = b"not valid json { { {";
        let result = parse_inbound(bad);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ParseError::Json(_)),
            "malformed JSON must produce ParseError::Json"
        );
    }

    #[test]
    fn parse_inbound_valid_message_succeeds() {
        let json = r#"{"type":"auth_required","ha_version":"2024.4.0"}"#;
        let msg = parse_inbound(json.as_bytes()).unwrap();
        assert!(matches!(msg, InboundMsg::AuthRequired(_)));
    }

    // -----------------------------------------------------------------------
    // Inbound: oversized frame
    // -----------------------------------------------------------------------

    #[test]
    fn oversized_frame_returns_typed_error() {
        // Build a synthetic JSON string that exceeds DEFAULT_PROFILE.ws_payload_cap.
        // We construct a payload slightly larger than the cap; the bytes are valid
        // UTF-8 but we never attempt to deserialize them — ParseError::OversizedFrame
        // is returned first.
        let cap = DEFAULT_PROFILE.ws_payload_cap;
        // 17 MiB > 16 MiB cap.
        let oversized = vec![b'x'; cap + 1];
        let result = parse_inbound(&oversized);
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::OversizedFrame {
                actual_bytes,
                cap_bytes,
            } => {
                assert_eq!(actual_bytes, cap + 1);
                assert_eq!(cap_bytes, cap);
            }
            other => panic!("expected OversizedFrame, got: {other:?}"),
        }
    }

    #[test]
    fn frame_at_exactly_cap_is_not_oversized() {
        // A frame of exactly ws_payload_cap bytes is not oversized — the guard
        // is strictly greater-than.  This byte slice is not valid JSON, so it
        // falls through to ParseError::Json, but crucially not OversizedFrame.
        let cap = DEFAULT_PROFILE.ws_payload_cap;
        let exactly_cap = vec![b'x'; cap];
        let result = parse_inbound(&exactly_cap);
        assert!(result.is_err());
        // Must be Json error, not OversizedFrame.
        assert!(
            matches!(result.unwrap_err(), ParseError::Json(_)),
            "frame at exactly cap must not produce OversizedFrame"
        );
    }

    // -----------------------------------------------------------------------
    // HaError: round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn ha_error_round_trip() {
        let err = HaError {
            code: "unknown_error".to_owned(),
            message: "Something went wrong".to_owned(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let decoded: HaError = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, err);
    }

    // -----------------------------------------------------------------------
    // ParseError::oversized convenience constructor
    // -----------------------------------------------------------------------

    #[test]
    fn parse_error_oversized_uses_default_profile_cap() {
        let err = ParseError::oversized(999_999_999);
        match err {
            ParseError::OversizedFrame {
                actual_bytes,
                cap_bytes,
            } => {
                assert_eq!(actual_bytes, 999_999_999);
                assert_eq!(cap_bytes, DEFAULT_PROFILE.ws_payload_cap);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // RawEntityState: Debug redacts attributes
    // -----------------------------------------------------------------------

    #[test]
    fn raw_entity_state_debug_redacts_attributes() {
        let state = RawEntityState {
            entity_id: "light.kitchen".to_owned(),
            state: "on".to_owned(),
            attributes: serde_json::json!({"brightness": 255, "secret_code": "xyzzy"}),
            last_changed: "2024-04-01T12:00:00+00:00".to_owned(),
            last_updated: "2024-04-01T12:00:00+00:00".to_owned(),
        };
        let debug_str = format!("{state:?}");
        assert!(
            !debug_str.contains("xyzzy"),
            "attributes must not appear in Debug output; got: {debug_str}"
        );
        assert!(
            !debug_str.contains("255"),
            "attribute values must not appear in Debug output; got: {debug_str}"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]; got: {debug_str}"
        );
        // entity_id and state are safe diagnostic identifiers — they must appear.
        assert!(
            debug_str.contains("light.kitchen"),
            "entity_id must appear in Debug output; got: {debug_str}"
        );
        assert!(
            debug_str.contains("\"on\""),
            "state must appear in Debug output; got: {debug_str}"
        );
    }
}
