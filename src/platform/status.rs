//! Home Assistant connection state and broadcast channel.
//!
//! # Connection lifecycle
//!
//! The FSM follows this progression (happy path):
//!
//! ```text
//! Connecting → Authenticating → Subscribing → Snapshotting → Services → Live
//! ```
//!
//! On transport error, the FSM transitions to `Reconnecting` and loops back to
//! `Connecting` after exponential backoff (TASK-032).  On `auth_invalid`, the
//! FSM transitions to `Failed` and does not reconnect.
//!
//! # Watch-channel late-joiner semantics (Risk #10)
//!
//! The channel returned by [`channel`] uses [`tokio::sync::watch`], which
//! delivers the **current value** to subscribers at the time they call
//! `receiver.borrow()` or `receiver.changed().await`.  A late-joiner that
//! subscribes after the sequence `Live → Reconnecting → Live` will see only the
//! current `Live` value — it will **not** observe the intermediate
//! `Reconnecting` transition.
//!
//! This is correct for Phase 2's bridge use case, which gates on the current
//! state (not transitions).
//!
//! **Phase 3 caveat**: any Phase 3 code that needs to react to the
//! `Reconnecting` transition itself (rather than the current state) MUST use a
//! different signaling mechanism — for example, a `tokio::sync::broadcast`
//! channel, a `tokio::sync::Notify`, or an event queue.  Using this watch
//! channel for transition detection will silently miss events.

use tokio::sync::watch;

/// Current phase of the Home Assistant WebSocket connection lifecycle.
///
/// The FSM is driven by `src/ha/client.rs` (TASK-029) and published via the
/// watch channel returned by [`channel`].  Consumers (e.g. `src/ui/bridge.rs`,
/// TASK-033) read the current state to gate property writes and control the
/// status banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// TCP/WebSocket connection is being established.
    Connecting,
    /// WebSocket connected; HA `auth_required` received; sending `auth` frame.
    Authenticating,
    /// Authenticated; sending `subscribe_events` and awaiting the ACK reply.
    Subscribing,
    /// Subscription ACK received; `get_states` issued; receiving snapshot data.
    Snapshotting,
    /// Snapshot applied; `get_services` issued; awaiting service registry reply.
    Services,
    /// Fully operational: snapshot applied, events flowing, services cached.
    Live,
    /// Transport error or `auth_required` mid-session; reconnecting with
    /// exponential backoff (TASK-032).
    Reconnecting,
    /// `auth_invalid` received, or overflow circuit-breaker tripped (3
    /// consecutive snapshot-buffer overflows within 60 s).  No automatic
    /// reconnect; surface a human-readable error in the status banner.
    Failed,
}

/// Create a new watch channel seeded with [`ConnectionState::Connecting`].
///
/// The sender half is owned by `src/ha/client.rs` (TASK-029), which drives FSM
/// transitions.  The receiver half is cloned freely — each clone shares the
/// same underlying channel and always reads the current value.
///
/// # Late-joiner semantics
///
/// Receivers see only the current value, not historical transitions.  See the
/// module-level doc and Risk #10 in `docs/plans/2026-04-26-phase-2-live-state.md`
/// for the Phase 3 caveat.
pub fn channel() -> (
    watch::Sender<ConnectionState>,
    watch::Receiver<ConnectionState>,
) {
    watch::channel(ConnectionState::Connecting)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Enum derives
    // -----------------------------------------------------------------------

    #[test]
    fn connection_state_debug_is_human_readable() {
        assert_eq!(format!("{:?}", ConnectionState::Live), "Live");
        assert_eq!(format!("{:?}", ConnectionState::Failed), "Failed");
        assert_eq!(
            format!("{:?}", ConnectionState::Reconnecting),
            "Reconnecting"
        );
    }

    #[test]
    fn connection_state_clone_equals_original() {
        let s = ConnectionState::Authenticating;
        #[allow(clippy::clone_on_copy)]
        let cloned = s.clone();
        assert_eq!(s, cloned);
    }

    #[test]
    fn connection_state_copy_is_bitwise() {
        let s = ConnectionState::Snapshotting;
        let copied = s; // Copy, not Clone
        assert_eq!(s, copied);
    }

    #[test]
    fn connection_state_eq_holds_for_identical_variants() {
        assert_eq!(ConnectionState::Live, ConnectionState::Live);
        assert_ne!(ConnectionState::Live, ConnectionState::Failed);
    }

    #[test]
    fn all_variants_are_distinct() {
        let variants = [
            ConnectionState::Connecting,
            ConnectionState::Authenticating,
            ConnectionState::Subscribing,
            ConnectionState::Snapshotting,
            ConnectionState::Services,
            ConnectionState::Live,
            ConnectionState::Reconnecting,
            ConnectionState::Failed,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Watch channel — basic shape
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn channel_initial_state_is_connecting() {
        let (_tx, rx) = channel();
        assert_eq!(*rx.borrow(), ConnectionState::Connecting);
    }

    #[tokio::test]
    async fn channel_send_recv_state_transition() {
        let (tx, mut rx) = channel();

        tx.send(ConnectionState::Live).unwrap();
        rx.changed().await.unwrap();

        assert_eq!(*rx.borrow_and_update(), ConnectionState::Live);
    }

    #[tokio::test]
    async fn channel_receiver_sees_all_transitions_when_polling_synchronously() {
        let (tx, mut rx) = channel();

        tx.send(ConnectionState::Authenticating).unwrap();
        tx.send(ConnectionState::Live).unwrap();

        // watch channel keeps only the latest value; after two sends the receiver
        // will see only one changed() notification (the latest send).
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), ConnectionState::Live);
    }

    // -----------------------------------------------------------------------
    // Late-joiner semantics
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn late_joiner_sees_current_state_not_history() {
        let (tx, _rx_early) = channel();

        // Drive through several states.
        tx.send(ConnectionState::Authenticating).unwrap();
        tx.send(ConnectionState::Live).unwrap();
        tx.send(ConnectionState::Reconnecting).unwrap();
        tx.send(ConnectionState::Live).unwrap();

        // A receiver created after all transitions sees only the current value.
        let rx_late = tx.subscribe();
        assert_eq!(
            *rx_late.borrow(),
            ConnectionState::Live,
            "late joiner must see current state, not historical transitions"
        );
    }

    #[tokio::test]
    async fn sender_can_clone_receiver() {
        let (tx, rx1) = channel();
        let rx2 = tx.subscribe();

        tx.send(ConnectionState::Failed).unwrap();

        assert_eq!(*rx1.borrow(), ConnectionState::Failed);
        assert_eq!(*rx2.borrow(), ConnectionState::Failed);
    }
}
