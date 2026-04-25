pub mod dashboard;

use anyhow::Result;
use tracing::info;

/// Orchestration entry point called by `main.rs`.
///
/// At this stage the function builds the Tokio runtime from `DEFAULT_PROFILE`
/// and parks itself; subsequent tasks (TASK-005 onward) will populate the
/// module tree and wire the event loop here.
pub fn run() -> Result<()> {
    use dashboard::profiles::DEFAULT_PROFILE;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(DEFAULT_PROFILE.tokio_workers)
        .enable_all()
        .build()?;

    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    info!("hanui starting");
    Ok(())
}
