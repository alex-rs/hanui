//! Platform-level services: configuration loading and connection status.
//!
//! # Modules
//!
//! - [`config`] — env+config-file precedence loader for `HA_URL` and `HA_TOKEN`.
//!   The token is stored as [`secrecy::SecretString`] and never exposed in logs.
//! - [`status`] — [`ConnectionState`][status::ConnectionState] enum and
//!   [`tokio::sync::watch`] channel for broadcasting current connection state.

pub mod config;
pub mod status;
