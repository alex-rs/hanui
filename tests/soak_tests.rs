//! Top-level soak test binary that pulls in `tests/soak/`.
//!
//! Feature-gated: this binary only compiles useful tests when
//! `--features soak` is passed.  Without the feature the binary
//! compiles but contains no test functions.
//!
//! Run the soak suite:
//! ```sh
//! cargo test --features soak --test soak_tests -- --nocapture
//! ```
//!
//! See `tests/soak/memory.rs` for the 10-minute memory soak scenario.

// `tests/common/mock_ws.rs` is shared mock infrastructure; the soak binary uses
// only a subset of MockWsServer's API (the rest is exercised by other test
// binaries). `#[expect(dead_code)]` is cleaner than a per-method `allow` and
// is self-cleaning if soak ever uses every method.
#[expect(dead_code)]
#[path = "common/mod.rs"]
mod common;

#[path = "soak/mod.rs"]
mod soak;
