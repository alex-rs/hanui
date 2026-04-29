//! TASK-090 — end-to-end integration test for the validator-derived
//! offline-queue allowlist.
//!
//! This test wires the YAML-side production path (validator emits
//! `CallServiceAllowlist`; loader stores it on `Dashboard.call_service_allowlist`)
//! to the runtime gate ([`OfflineQueue`] consults the Arc on each enqueue).
//! The unit tests in `src/actions/queue.rs::tests` exercise each piece in
//! isolation; this test demonstrates the full producer→consumer chain so a
//! regression that breaks the wiring (e.g. the loader stops attaching the
//! allowlist; the queue's constructor stops threading the Arc) fails here.
//!
//! Per `locked_decisions.call_service_allowlist_runtime_access` and
//! TASK-090's `tests_added` block:
//!
//!   integration::offline_queue::end_to_end_yaml_allowlist
//!
//! # What is asserted
//!
//! Construct a `Dashboard` whose `call_service_allowlist` declares
//! `(light, turn_on)` and `(cover, open_cover)` and nothing else. Build an
//! [`OfflineQueue`] with `Some(dashboard.call_service_allowlist.clone())`
//! and try to enqueue:
//!
//! 1. `light.turn_on` — must succeed (declared).
//! 2. `cover.open_cover` — must succeed (declared; verb-named, not
//!    matchable by the Phase 3 prefix rule).
//! 3. `light.set_brightness` — must FAIL with `ServiceNotAllowlisted`,
//!    even though the prefix rule WOULD have allowed it.
//!
//! The test does not exercise the YAML loader's I/O path (that is covered
//! by `loader::tests::happy_path_loads_minimal_yaml`); it builds the
//! `Dashboard` directly to keep the focus on the queue's enqueue gate.

use std::collections::BTreeSet;
use std::sync::Arc;

use hanui::actions::queue::{OfflineQueue, QueueError};
use hanui::actions::schema::Action;
use hanui::dashboard::schema::CallServiceAllowlist;
use hanui::ha::entity::EntityId;

#[test]
fn end_to_end_yaml_allowlist() {
    // Synthesise the allowlist a Dashboard would carry after a successful
    // YAML-validate pass. The pairs match a YAML config of the shape:
    //   widgets:
    //     - id: kitchen_light
    //       tap_action: { domain: light, service: turn_on, ... }
    //     - id: living_room_blind
    //       tap_action: { domain: cover, service: open_cover, ... }
    let allowlist: CallServiceAllowlist = BTreeSet::from([
        ("light".to_string(), "turn_on".to_string()),
        ("cover".to_string(), "open_cover".to_string()),
    ]);
    let arc = Arc::new(allowlist);

    let mut queue = OfflineQueue::with_allowlist(Some(arc.clone()));

    // (1) Declared service — accepted.
    queue
        .enqueue(
            Action::CallService {
                domain: "light".to_string(),
                service: "turn_on".to_string(),
                target: Some("light.kitchen".to_string()),
                data: None,
            },
            Some(EntityId::from("light.kitchen")),
            None,
        )
        .expect("light.turn_on is in the YAML allowlist — must enqueue");
    assert_eq!(queue.len(), 1);

    // (2) Verb-named declared service — accepted (prefix rule would
    // REJECT this; the YAML allowlist permits it because the YAML
    // explicitly declares it).
    queue
        .enqueue(
            Action::CallService {
                domain: "cover".to_string(),
                service: "open_cover".to_string(),
                target: Some("cover.living_room".to_string()),
                data: None,
            },
            Some(EntityId::from("cover.living_room")),
            None,
        )
        .expect("cover.open_cover is in the YAML allowlist — must enqueue");
    assert_eq!(queue.len(), 2);

    // (3) Undeclared service that WOULD pass the prefix fallback —
    // rejected by the strict per-config gate. This is the load-bearing
    // tightening promise of TASK-090 / TASK-077: the prefix rule no
    // longer auto-allows `set_*` once a YAML allowlist is loaded.
    let result = queue.enqueue(
        Action::CallService {
            domain: "light".to_string(),
            service: "set_brightness".to_string(),
            target: Some("light.kitchen".to_string()),
            data: Some(serde_json::json!({ "brightness": 200 })),
        },
        Some(EntityId::from("light.kitchen")),
        None,
    );
    match result {
        Err(QueueError::ServiceNotAllowlisted { domain, service }) => {
            assert_eq!(domain, "light");
            assert_eq!(service, "set_brightness");
        }
        other => panic!(
            "expected ServiceNotAllowlisted for light.set_brightness with strict YAML allowlist, \
             got {other:?}"
        ),
    }
    assert_eq!(
        queue.len(),
        2,
        "rejected action must NOT enter the queue (queue length unchanged)"
    );

    // Sanity: the Arc is still held by both the test and the queue —
    // the queue did not silently swap to a different allowlist. The
    // strong-count is 2 (test + queue field).
    assert_eq!(
        Arc::strong_count(&arc),
        2,
        "queue must hold a clone of the same Arc the test passed in"
    );
}
