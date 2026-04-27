//! Home Assistant service registry.
//!
//! This module provides [`ServiceRegistry`], a typed in-memory cache of the
//! service definitions returned by the HA `get_services` WebSocket command.
//!
//! # Lifecycle
//!
//! On every successful WebSocket connection, the client (TASK-029) issues a
//! `get_services` command and feeds the reply payload to
//! [`ServiceRegistry::from_get_services_result`].  The resulting registry is
//! handed to Phase 3's action dispatcher, which calls [`ServiceRegistry::lookup`]
//! to validate `(domain, service)` pairs before dispatching tap-actions.
//!
//! The registry is kept fresh between full re-fetches by incremental updates
//! driven from the WS event stream (TASK-049):
//! - [`ServiceRegistry::add_service`] is invoked from `src/ha/client.rs`
//!   when a `service_registered` event arrives in `Phase::Live`.  HA does not
//!   include the service's field schema in the event payload, so the
//!   incremental entry is added with a default-empty [`ServiceMeta`]; the next
//!   successful reconnect's `get_services` round refills the metadata.
//!   Lookups in the meantime return `Some(default_meta)`, which is the correct
//!   "exists but schema unknown" signal — Phase 3 dispatchers can still
//!   resolve the `(domain, service)` pair and issue a `call_service` frame
//!   without parameter introspection.
//! - [`ServiceRegistry::remove_service`] is invoked when a `service_removed`
//!   event arrives in `Phase::Live`.  Subsequent lookups for that pair return
//!   `None`, matching the partial-failure contract below.
//!
//! Subscription wiring: TASK-049 changed the client's single
//! `subscribe_events` frame to omit `event_type`, subscribing to ALL events on
//! the bus.  HA's WS API documents this as the canonical way to receive
//! multiple event types over one subscription; the FSM dispatches by
//! `EventVariant` internally.  Trade-off: the client now sees every event
//! type HA emits (most are ignored as `EventVariant::Other`), in exchange for
//! a single ACK gate, no FSM phase explosion, and zero new sequencing
//! invariants beyond the one that already exists.
//!
//! # Partial-failure handling (Risk #12)
//!
//! If `get_services` errors, times out, or returns a payload that fails to
//! parse, the registry is left in its default empty state.  [`lookup`] returns
//! `None` for every query in that state, which causes Phase 3's dispatcher to
//! reject all tap-actions gracefully rather than acting on stale data.  The next
//! successful reconnect cycle re-issues `get_services` and refreshes the
//! registry.  No special "is_empty" sentinel is needed — `None` from `lookup`
//! is the correct rejection signal.
//!
//! Mid-session `service_registered` / `service_removed` events keep the
//! registry fresh between full re-fetches; the "stale until reconnect"
//! window therefore shrinks from "the lifetime of the WS session" to "the
//! latency of one event delivery".
//!
//! [`lookup`]: ServiceRegistry::lookup

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::ha::protocol::ParseError;

// ---------------------------------------------------------------------------
// ServiceRegistryHandle
// ---------------------------------------------------------------------------

/// Shared, thread-safe handle to a [`ServiceRegistry`].
///
/// Constructed once in `src/lib.rs::build_ws_client_with_store` (TASK-048),
/// then cloned into both the [`WsClient`] (which writes via the `Services →
/// Live` transition) and the [`LiveStore`] (which exposes a read accessor so
/// Phase 3 dispatchers can validate `(domain, service)` pairs without holding a
/// `WsClient` handle).
///
/// `Arc<RwLock<_>>` gives `Send + Sync + Clone` for free; see the compile-time
/// assertion in [`ServiceRegistry`]'s test module.
///
/// [`WsClient`]: crate::ha::client::WsClient
/// [`LiveStore`]: crate::ha::live_store::LiveStore
pub type ServiceRegistryHandle = Arc<RwLock<ServiceRegistry>>;

// ---------------------------------------------------------------------------
// ParseError re-export (services uses the same error type as protocol)
// ---------------------------------------------------------------------------

/// Error produced when a `get_services` payload cannot be parsed.
///
/// Wraps [`ParseError`] so callers have a single error type to match and the
/// services module is consistent with the rest of the HA protocol layer.
#[derive(Debug, Error)]
pub enum ServicesParseError {
    /// The JSON value did not have the expected object structure.
    #[error("get_services result is not a JSON object")]
    NotAnObject,
    /// A domain's service map was not a JSON object.
    #[error("service map for domain `{domain}` is not a JSON object")]
    DomainNotAnObject { domain: String },
    /// JSON deserialize error propagated from the protocol layer.
    #[error("JSON deserialize error: {0}")]
    Json(#[from] serde_json::Error),
    /// Protocol-level parse error (e.g. oversized frame).
    #[error(transparent)]
    Protocol(#[from] ParseError),
}

// ---------------------------------------------------------------------------
// ServiceField
// ---------------------------------------------------------------------------

/// Metadata for a single field of a Home Assistant service call.
///
/// Fields are keyed by name inside [`ServiceMeta::fields`].  All fields
/// are optional at the protocol level — HA may omit any of them for simpler
/// services, so every sub-field here is `Option`.
#[derive(Debug, Clone, Default)]
pub struct ServiceField {
    /// Human-readable description of the field.
    pub description: Option<String>,
    /// An example value for the field, in the type HA would accept.
    pub example: Option<serde_json::Value>,
    /// Whether this field is required for the service call.
    pub required: bool,
}

// ---------------------------------------------------------------------------
// ServiceMeta
// ---------------------------------------------------------------------------

/// Metadata for a single Home Assistant service.
///
/// Obtained from the `get_services` payload under `domain → service_name →
/// service_meta`.  Phase 3's action dispatcher uses this to validate
/// tap-action targets and their parameters.
#[derive(Debug, Clone, Default)]
pub struct ServiceMeta {
    /// Human-readable name of the service (may differ from the map key).
    pub name: String,
    /// Human-readable description of what the service does.
    pub description: Option<String>,
    /// Named fields accepted by the service call.
    pub fields: HashMap<String, ServiceField>,
}

// ---------------------------------------------------------------------------
// ServiceRegistry
// ---------------------------------------------------------------------------

/// In-memory cache of Home Assistant service definitions.
///
/// Keyed as `domain → service_name → ServiceMeta`.  Populated from a
/// `get_services` reply and updated incrementally from `service_registered` /
/// `service_removed` events.
///
/// See module-level doc for the partial-failure contract.
#[derive(Debug, Default)]
pub struct ServiceRegistry {
    /// Outer key: HA domain (e.g. `"light"`, `"switch"`, `"script"`).
    /// Inner key: service name within the domain (e.g. `"turn_on"`, `"turn_off"`).
    services: HashMap<String, HashMap<String, ServiceMeta>>,
}

impl ServiceRegistry {
    /// Create a new, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new, empty registry wrapped in a [`ServiceRegistryHandle`].
    ///
    /// Convenience constructor used by `src/lib.rs::run_with_live_store` and by
    /// the integration tests so callers don't need to import `Arc` and `RwLock`
    /// just to wire a fresh, empty handle.  Preferred over inlining
    /// `Arc::new(RwLock::new(ServiceRegistry::new()))` at every call site.
    pub fn new_handle() -> ServiceRegistryHandle {
        Arc::new(RwLock::new(Self::new()))
    }

    /// Parse a `get_services` result payload into a populated registry.
    ///
    /// The `value` argument is the `result` field from the HA `result` frame
    /// (i.e. `ResultPayload::result`).  HA defines the shape as:
    ///
    /// ```json
    /// {
    ///   "domain": {
    ///     "service_name": {
    ///       "name": "...",
    ///       "description": "...",
    ///       "fields": {
    ///         "field_name": {
    ///           "description": "...",
    ///           "example": <any>,
    ///           "required": <bool>
    ///         }
    ///       }
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// Unknown keys at any level are silently ignored to tolerate future HA
    /// additions.  Fields that are absent from the payload default to their
    /// `Default` values (`None` / `false`).
    ///
    /// Returns [`ServicesParseError::NotAnObject`] if the top-level value is
    /// not a JSON object.  Returns [`ServicesParseError::DomainNotAnObject`] if
    /// a domain's value is not a JSON object.
    pub fn from_get_services_result(value: &serde_json::Value) -> Result<Self, ServicesParseError> {
        let top = value.as_object().ok_or(ServicesParseError::NotAnObject)?;

        let mut registry = Self::new();

        for (domain, domain_val) in top {
            let svc_map =
                domain_val
                    .as_object()
                    .ok_or_else(|| ServicesParseError::DomainNotAnObject {
                        domain: domain.clone(),
                    })?;

            let domain_entry = registry.services.entry(domain.clone()).or_default();

            for (svc_name, svc_val) in svc_map {
                let meta = parse_service_meta(svc_val);
                domain_entry.insert(svc_name.clone(), meta);
            }
        }

        Ok(registry)
    }

    /// Look up a service by `(domain, service)` pair.
    ///
    /// Returns `None` if either the domain or the service within that domain
    /// is not present.  An empty registry returns `None` for every query.
    pub fn lookup(&self, domain: &str, service: &str) -> Option<&ServiceMeta> {
        self.services.get(domain)?.get(service)
    }

    /// Incrementally add or replace a service entry.
    ///
    /// Applied on `service_registered` events from the HA event stream.
    /// If the domain does not yet exist in the registry it is created.
    pub fn add_service(&mut self, domain: &str, service: &str, meta: ServiceMeta) {
        self.services
            .entry(domain.to_owned())
            .or_default()
            .insert(service.to_owned(), meta);
    }

    /// Incrementally remove a service entry.
    ///
    /// Applied on `service_removed` events from the HA event stream.
    /// No-ops silently if the domain or service is not present.
    pub fn remove_service(&mut self, domain: &str, service: &str) {
        if let Some(domain_map) = self.services.get_mut(domain) {
            domain_map.remove(service);
            // Leave the (now-possibly-empty) domain entry rather than removing
            // it — empty domain maps are harmless and avoids an extra allocation
            // on re-registration within the same domain.
        }
    }
}

// ---------------------------------------------------------------------------
// Internal parsing helpers
// ---------------------------------------------------------------------------

/// Parse a single service-meta JSON object into a [`ServiceMeta`].
///
/// Unknown keys are ignored; all fields default to `None` / `false` if absent.
fn parse_service_meta(v: &serde_json::Value) -> ServiceMeta {
    let mut meta = ServiceMeta::default();

    if let Some(obj) = v.as_object() {
        if let Some(name) = obj.get("name").and_then(|n| n.as_str()) {
            meta.name = name.to_owned();
        }
        if let Some(desc) = obj.get("description").and_then(|d| d.as_str()) {
            meta.description = Some(desc.to_owned());
        }
        if let Some(fields_obj) = obj.get("fields").and_then(|f| f.as_object()) {
            for (field_name, field_val) in fields_obj {
                meta.fields
                    .insert(field_name.clone(), parse_service_field(field_val));
            }
        }
    }

    meta
}

/// Parse a single service-field JSON object into a [`ServiceField`].
fn parse_service_field(v: &serde_json::Value) -> ServiceField {
    let mut field = ServiceField::default();

    if let Some(obj) = v.as_object() {
        if let Some(desc) = obj.get("description").and_then(|d| d.as_str()) {
            field.description = Some(desc.to_owned());
        }
        if let Some(example) = obj.get("example") {
            field.example = Some(example.clone());
        }
        if let Some(required) = obj.get("required").and_then(|r| r.as_bool()) {
            field.required = required;
        }
    }

    field
}

// ---------------------------------------------------------------------------
// Compile-time Send + Sync assertion
// ---------------------------------------------------------------------------

/// Compile-time assertion that [`ServiceRegistry`] is `Send + Sync`.
///
/// Phase 3's dispatcher will hold the registry behind an `Arc<RwLock<>>` shared
/// across Tokio tasks.  If `ServiceRegistry` ever gains a non-Send/Sync field
/// this function body will fail to compile, surfacing the regression immediately.
///
/// Called from tests so coverage instrumentation reaches these lines.
fn _assert_service_registry_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<ServiceRegistry>();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// A representative `get_services` payload with 2 domains × 2 services each.
    fn fixture_get_services_payload() -> serde_json::Value {
        serde_json::json!({
            "light": {
                "turn_on": {
                    "name": "Turn on",
                    "description": "Turn on one or more lights.",
                    "fields": {
                        "entity_id": {
                            "description": "The entity ID of the light.",
                            "example": "light.kitchen",
                            "required": true
                        },
                        "brightness": {
                            "description": "Number between 0 and 255 indicating light level.",
                            "example": 120,
                            "required": false
                        }
                    }
                },
                "turn_off": {
                    "name": "Turn off",
                    "description": "Turn off one or more lights.",
                    "fields": {
                        "entity_id": {
                            "description": "The entity ID of the light.",
                            "example": "light.kitchen",
                            "required": true
                        }
                    }
                }
            },
            "switch": {
                "turn_on": {
                    "name": "Turn on",
                    "description": "Turn on a switch.",
                    "fields": {
                        "entity_id": {
                            "description": "The entity ID of the switch.",
                            "example": "switch.outlet",
                            "required": true
                        }
                    }
                },
                "turn_off": {
                    "name": "Turn off",
                    "description": "Turn off a switch.",
                    "fields": {
                        "entity_id": {
                            "description": "The entity ID of the switch.",
                            "example": "switch.outlet",
                            "required": true
                        }
                    }
                }
            }
        })
    }

    // -----------------------------------------------------------------------
    // from_get_services_result: happy path
    // -----------------------------------------------------------------------

    #[test]
    fn parses_representative_payload_with_two_domains_two_services_each() {
        let payload = fixture_get_services_payload();
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();

        // Both domains must be present.
        assert!(
            registry.lookup("light", "turn_on").is_some(),
            "light.turn_on must be present"
        );
        assert!(
            registry.lookup("light", "turn_off").is_some(),
            "light.turn_off must be present"
        );
        assert!(
            registry.lookup("switch", "turn_on").is_some(),
            "switch.turn_on must be present"
        );
        assert!(
            registry.lookup("switch", "turn_off").is_some(),
            "switch.turn_off must be present"
        );
    }

    #[test]
    fn parses_service_meta_fields_correctly() {
        let payload = fixture_get_services_payload();
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();

        let meta = registry.lookup("light", "turn_on").unwrap();
        assert_eq!(meta.name, "Turn on");
        assert_eq!(
            meta.description.as_deref(),
            Some("Turn on one or more lights.")
        );

        let field = meta.fields.get("brightness").unwrap();
        assert_eq!(
            field.description.as_deref(),
            Some("Number between 0 and 255 indicating light level.")
        );
        assert_eq!(field.example, Some(serde_json::json!(120)));
        assert!(!field.required, "brightness must not be required");

        let required_field = meta.fields.get("entity_id").unwrap();
        assert!(required_field.required, "entity_id must be required");
    }

    #[test]
    fn parses_empty_object_into_empty_registry() {
        let payload = serde_json::json!({});
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();
        assert!(
            registry.lookup("light", "turn_on").is_none(),
            "empty payload must produce empty registry"
        );
    }

    // -----------------------------------------------------------------------
    // from_get_services_result: validation failures
    // -----------------------------------------------------------------------

    #[test]
    fn returns_error_when_payload_is_not_an_object() {
        let payload = serde_json::json!([1, 2, 3]);
        let result = ServiceRegistry::from_get_services_result(&payload);
        assert!(result.is_err(), "array payload must produce an error");
        assert!(
            matches!(result.unwrap_err(), ServicesParseError::NotAnObject),
            "error must be NotAnObject"
        );
    }

    #[test]
    fn returns_error_when_domain_value_is_not_an_object() {
        let payload = serde_json::json!({
            "light": "this_should_be_an_object"
        });
        let result = ServiceRegistry::from_get_services_result(&payload);
        assert!(result.is_err());
        match result.unwrap_err() {
            ServicesParseError::DomainNotAnObject { domain } => {
                assert_eq!(domain, "light");
            }
            other => panic!("expected DomainNotAnObject, got: {other:?}"),
        }
    }

    #[test]
    fn payload_is_null_returns_error() {
        let payload = serde_json::Value::Null;
        let result = ServiceRegistry::from_get_services_result(&payload);
        assert!(result.is_err(), "null payload must produce an error");
        assert!(matches!(
            result.unwrap_err(),
            ServicesParseError::NotAnObject
        ));
    }

    // -----------------------------------------------------------------------
    // lookup
    // -----------------------------------------------------------------------

    #[test]
    fn lookup_returns_some_for_known_domain_and_service() {
        let payload = fixture_get_services_payload();
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();

        let meta = registry.lookup("switch", "turn_off");
        assert!(meta.is_some(), "known (domain, service) must return Some");
        assert_eq!(meta.unwrap().name, "Turn off");
    }

    #[test]
    fn lookup_returns_none_for_unknown_domain() {
        let payload = fixture_get_services_payload();
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();

        let result = registry.lookup("climate", "set_temperature");
        assert!(result.is_none(), "unknown domain must return None");
    }

    #[test]
    fn lookup_returns_none_for_unknown_service_in_known_domain() {
        let payload = fixture_get_services_payload();
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();

        let result = registry.lookup("light", "flash");
        assert!(
            result.is_none(),
            "unknown service in known domain must return None"
        );
    }

    #[test]
    fn lookup_on_empty_registry_returns_none() {
        let registry = ServiceRegistry::new();
        assert!(
            registry.lookup("light", "turn_on").is_none(),
            "empty registry must return None for any lookup"
        );
    }

    // -----------------------------------------------------------------------
    // add_service
    // -----------------------------------------------------------------------

    #[test]
    fn add_service_makes_entry_visible_via_lookup() {
        let mut registry = ServiceRegistry::new();

        let meta = ServiceMeta {
            name: "Toggle".to_owned(),
            description: Some("Toggle a light.".to_owned()),
            fields: HashMap::new(),
        };
        registry.add_service("light", "toggle", meta);

        let found = registry.lookup("light", "toggle");
        assert!(found.is_some(), "added service must be visible via lookup");
        assert_eq!(found.unwrap().name, "Toggle");
    }

    #[test]
    fn add_service_creates_domain_entry_if_absent() {
        let mut registry = ServiceRegistry::new();

        let meta = ServiceMeta {
            name: "Turn on".to_owned(),
            description: None,
            fields: HashMap::new(),
        };
        registry.add_service("climate", "turn_on", meta);

        assert!(
            registry.lookup("climate", "turn_on").is_some(),
            "new domain must be created by add_service"
        );
    }

    #[test]
    fn add_service_replaces_existing_entry() {
        let mut registry = ServiceRegistry::new();

        let meta_v1 = ServiceMeta {
            name: "Old name".to_owned(),
            description: None,
            fields: HashMap::new(),
        };
        registry.add_service("light", "turn_on", meta_v1);

        let meta_v2 = ServiceMeta {
            name: "New name".to_owned(),
            description: Some("Updated description.".to_owned()),
            fields: HashMap::new(),
        };
        registry.add_service("light", "turn_on", meta_v2);

        let found = registry.lookup("light", "turn_on").unwrap();
        assert_eq!(found.name, "New name");
        assert_eq!(found.description.as_deref(), Some("Updated description."));
    }

    // -----------------------------------------------------------------------
    // remove_service
    // -----------------------------------------------------------------------

    #[test]
    fn remove_service_makes_entry_invisible_via_lookup() {
        let payload = fixture_get_services_payload();
        let mut registry = ServiceRegistry::from_get_services_result(&payload).unwrap();

        registry.remove_service("light", "turn_on");

        assert!(
            registry.lookup("light", "turn_on").is_none(),
            "removed service must not be visible via lookup"
        );
        // Other services in the same domain must be unaffected.
        assert!(
            registry.lookup("light", "turn_off").is_some(),
            "unremoved service in same domain must remain visible"
        );
    }

    #[test]
    fn remove_service_is_noop_for_unknown_domain() {
        let mut registry = ServiceRegistry::new();
        // Must not panic.
        registry.remove_service("climate", "turn_on");
    }

    #[test]
    fn remove_service_is_noop_for_unknown_service_in_known_domain() {
        let payload = fixture_get_services_payload();
        let mut registry = ServiceRegistry::from_get_services_result(&payload).unwrap();

        // Must not panic.
        registry.remove_service("light", "nonexistent_service");

        // Existing services must be unaffected.
        assert!(registry.lookup("light", "turn_on").is_some());
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn service_with_no_fields_key_parses_to_empty_fields_map() {
        let payload = serde_json::json!({
            "script": {
                "my_script": {
                    "name": "My Script",
                    "description": "A script with no parameters."
                }
            }
        });
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();
        let meta = registry.lookup("script", "my_script").unwrap();
        assert!(
            meta.fields.is_empty(),
            "service with no fields must have empty fields map"
        );
    }

    #[test]
    fn service_with_no_name_key_defaults_to_empty_string() {
        let payload = serde_json::json!({
            "homeassistant": {
                "reload": {}
            }
        });
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();
        let meta = registry.lookup("homeassistant", "reload").unwrap();
        assert_eq!(
            meta.name, "",
            "absent name key must default to empty string"
        );
        assert!(meta.description.is_none());
    }

    #[test]
    fn service_field_with_no_required_key_defaults_to_false() {
        let payload = serde_json::json!({
            "light": {
                "turn_on": {
                    "name": "Turn on",
                    "fields": {
                        "color_temp": {
                            "description": "Color temperature in mireds.",
                            "example": 300
                        }
                    }
                }
            }
        });
        let registry = ServiceRegistry::from_get_services_result(&payload).unwrap();
        let meta = registry.lookup("light", "turn_on").unwrap();
        let field = meta.fields.get("color_temp").unwrap();
        assert!(!field.required, "absent required key must default to false");
    }

    #[test]
    fn unknown_top_level_service_keys_are_ignored() {
        let payload = serde_json::json!({
            "light": {
                "turn_on": {
                    "name": "Turn on",
                    "description": "Turn on a light.",
                    "fields": {},
                    "target": { "entity": [{"domain": "light"}] },
                    "future_key": "some_value_from_ha_next_version"
                }
            }
        });
        // Must not error on unknown keys.
        let result = ServiceRegistry::from_get_services_result(&payload);
        assert!(result.is_ok(), "unknown keys must be silently ignored");
        let registry = result.unwrap();
        assert!(registry.lookup("light", "turn_on").is_some());
    }

    // -----------------------------------------------------------------------
    // Compile-time Send + Sync — coverage exercise
    // -----------------------------------------------------------------------

    #[test]
    fn service_registry_send_sync_bound_proof_runs_clean() {
        // Calls the compile-time assertion function so coverage instrumentation
        // reaches it.  The test will fail to compile if ServiceRegistry ever
        // gains a non-Send or non-Sync field.
        _assert_service_registry_send_sync();
    }
}
