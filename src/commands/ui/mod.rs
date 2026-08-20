use crate::core::executor::discover_tests;
use crate::core::tree::build_flat_tree;
use anyhow::Result;

pub(crate) mod config;
mod discovery_cache;
mod failed_tests;
mod failure_summary;
mod filter;
mod interactive;
mod layout;
mod manual_watch;
mod output;
mod presets;
mod run_results;
mod test_run;

pub fn run() -> Result<()> {
    let config = config::RunConfig::load();

    eprintln!("[discovery] timing logs go to stderr (prefix [discovery])");
    let tests = if let Some(cached) = discovery_cache::try_load_cached_tests() {
        cached
    } else {
        println!("Discovering tests (this may take a moment)...");
        eprintln!(
            "[discovery] cache miss → full rediscovery (builds into bin/dotest/; skip-build is ignored for accuracy)"
        );
        // Discovery should be resilient on clean repos; forcing --no-build can
        // fail with "test source file ... was not found" when binaries aren't built yet.
        // Cache miss also means sources changed, so we must build to see new/renamed tests.
        let tests = discover_tests(false, config.no_restore)?;
        discovery_cache::save_discovery_cache(&tests)?;
        tests
    };

    if tests.is_empty() {
        println!("No tests found.");
        return Ok(());
    }
    let mut tree = build_flat_tree(&tests);
    if let Some(state) = discovery_cache::load_tree_state() {
        state.restore(&mut tree);
    }
    interactive::run_interactive_loop(&mut tree, config)
}
