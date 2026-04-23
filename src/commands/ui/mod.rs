use anyhow::Result;
use crate::core::executor::discover_tests;
use crate::core::tree::build_flat_tree;

pub(crate) mod config;
mod filter;
mod interactive;
mod layout;
mod manual_watch;
mod output;

pub fn run() -> Result<()> {
    let config = config::RunConfig::load();

    let tests = if config.cache_tests {
        if let Ok(s) = std::fs::read_to_string(".dotest_cache.json") {
            if let Ok(cached) = serde_json::from_str::<Vec<(String, String, usize)>>(&s) {
                if cached.is_empty() {
                    discover_and_cache(config.no_restore)?
                } else {
                    cached
                }
            } else {
                discover_and_cache(config.no_restore)?
            }
        } else {
            discover_and_cache(config.no_restore)?
        }
    } else {
        println!("Discovering tests (this may take a moment)...");
        discover_tests(true, config.no_restore)?
    };

    if tests.is_empty() {
        println!("No tests found.");
        return Ok(());
    }
    let mut tree = build_flat_tree(&tests);
    interactive::run_interactive_loop(&mut tree, config)
}

fn discover_and_cache(no_restore: bool) -> Result<Vec<(String, String, usize)>> {
    println!("Discovering tests (this may take a moment)...");
    let tests = discover_tests(true, no_restore)?;
    if let Ok(s) = serde_json::to_string(&tests) {
        let _ = std::fs::write(".dotest_cache.json", s);
    }
    Ok(tests)
}
