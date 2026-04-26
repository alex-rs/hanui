//! `hanui` binary entry point.
//!
//! All orchestration lives in [`hanui::run`].  This binary's only job is to
//! print a sanitised error message and exit with status 1 if `run` fails —
//! never panic on user-recoverable conditions like missing env vars (per
//! TASK-034 AC: "exits with a clear error message naming the missing config —
//! does NOT panic").
//!
//! `eprintln!` to stderr, exit code 1, is the conventional Unix CLI shape.
//! The error chain is rendered via `anyhow`'s `{:#}` formatter so the user
//! sees both the top-level context (e.g. "load HA connection config from env")
//! and the underlying cause (e.g. "required environment variable `HA_URL` is
//! not set") on a single line, no backtrace noise.

fn main() {
    if let Err(err) = hanui::run() {
        eprintln!("hanui: {err:#}");
        std::process::exit(1);
    }
}
