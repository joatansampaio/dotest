use crate::core::discovery::{discovery_log, set_discovery_log_stderr};
use crate::core::executor::{discover_tests, discover_tests_for_targets};
use crate::core::tree::build_flat_tree;
use anyhow::Result;

use discovery_cache::{
    has_anchored_tests, legacy_fingerprint_matches, live_fingerprints, load_cache,
    merge_discovered, plan_incremental, save_discovery_cache, save_discovery_cache_tracking,
    solution_fingerprint, IncrementalPlan, LoadedCache,
};

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

    // Timing is opt-in so F5 rediscovery cannot spam stderr into the TUI alternate screen.
    // DOTEST_DISCOVERY_TIMING=1 writes `.dotest_discovery.log` (and stderr before the TUI starts).
    if std::env::var_os("DOTEST_DISCOVERY_TIMING").is_some_and(|v| v != "0") {
        eprintln!("[discovery] timing enabled → `.dotest_discovery.log` (and stderr until TUI)");
    }

    let tests = discover_using_cache(config.no_restore, true)?;

    if tests.is_empty() {
        println!("No tests found.");
        return Ok(());
    }
    let mut tree = build_flat_tree(&tests);
    if let Some(state) = discovery_cache::load_tree_state() {
        state.restore(&mut tree);
    }
    // Prevent discovery timing lines from corrupting the alternate screen (esp. F5).
    set_discovery_log_stderr(false);
    let result = interactive::run_interactive_loop(&mut tree, config);
    set_discovery_log_stderr(true);
    result
}

/// Load the last correct test list, or rediscover only the projects that changed.
///
/// `announce` prints progress. Startup uses it; F5 does not, because stdout would
/// corrupt the alternate screen.
pub(crate) fn discover_using_cache(
    no_restore: bool,
    announce: bool,
) -> Result<Vec<crate::core::discovery::DiscoveredTest>> {
    match load_cache() {
        LoadedCache::Missing => full_discover(no_restore, announce),
        LoadedCache::Legacy {
            tests,
            fingerprint,
            roots,
        } => {
            if legacy_fingerprint_matches(&fingerprint, &roots) {
                discovery_log(format!(
                    "cache: HIT legacy fingerprint ({} tests)",
                    tests.len()
                ));
                if has_anchored_tests(&tests) {
                    discovery_log(
                        "cache: recording per-project baseline so the next edit can skip unchanged projects",
                    );
                    save_discovery_cache(&tests)?;
                }
                return Ok(tests);
            }
            discovery_log("cache: MISS legacy fingerprint → full discovery");
            full_discover(no_restore, announce)
        }
        LoadedCache::Baseline(baseline) => {
            let current_solution = solution_fingerprint();
            if current_solution != baseline.solution_fingerprint {
                discovery_log(
                    "baseline rejected: solution or build config changed → full discovery",
                );
                return full_discover(no_restore, announce);
            }
            let live = live_fingerprints(&baseline.project_fingerprints);
            match plan_incremental(&baseline.tests, &baseline.project_fingerprints, &live) {
                IncrementalPlan::Unchanged => {
                    discovery_log(format!(
                        "cache: HIT project baseline ({} tests, {} project(s))",
                        baseline.tests.len(),
                        baseline.project_fingerprints.len()
                    ));
                    Ok(baseline.tests)
                }
                IncrementalPlan::Partial { keep, refresh } if refresh.is_empty() => {
                    discovery_log(format!(
                        "baseline: removed projects, keeping {} cached test(s)",
                        keep.len()
                    ));
                    if keep.is_empty() {
                        let _ = std::fs::remove_file(discovery_cache::CACHE_PATH);
                        return Ok(keep);
                    }
                    save_discovery_cache(&keep)?;
                    Ok(keep)
                }
                IncrementalPlan::Partial { keep, refresh } => {
                    discovery_log(format!(
                        "baseline: keep {} cached test(s), rediscover {}",
                        keep.len(),
                        refresh.join(", ")
                    ));
                    if announce {
                        println!(
                            "Discovering {} changed test project(s), reusing {} cached tests...",
                            refresh.len(),
                            keep.len()
                        );
                    }
                    let refreshed = discover_tests_for_targets(&refresh, false, no_restore)?;
                    let tests = merge_discovered(keep, refreshed);
                    save_discovery_cache_tracking(&tests, &refresh)?;
                    Ok(tests)
                }
                IncrementalPlan::Full => {
                    discovery_log(
                        "baseline: full discovery (several projects changed, or nothing stable left to reuse)",
                    );
                    full_discover(no_restore, announce)
                }
            }
        }
    }
}

fn full_discover(
    no_restore: bool,
    announce: bool,
) -> Result<Vec<crate::core::discovery::DiscoveredTest>> {
    if announce {
        println!("Discovering tests (this may take a moment)...");
    }
    discovery_log("full discovery (builds into bin/dotest/; skip-build ignored for accuracy)");
    // Discovery should be resilient on clean repos; forcing --no-build can
    // fail with "test source file ... was not found" when binaries aren't built yet.
    // A full discovery means the baseline cannot be reused, so we must build.
    let tests = discover_tests(false, no_restore)?;
    save_discovery_cache(&tests)?;
    Ok(tests)
}
