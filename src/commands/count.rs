use anyhow::{Context, Result};
use crate::commands::ui::config::RunConfig;
use crate::core::count::{resolve_short_segment_to_prefix, sum_for_count_query, tree_under_prefix};
use crate::core::executor::discover_tests;

pub fn run(folder: String, no_build: bool) -> Result<()> {
    let run_config = RunConfig::load();
    let tests = discover_tests(no_build, run_config.no_restore)?;
    let (ptrim, sum, buckets) = if folder.contains('.') {
        let p = folder.trim().trim_end_matches('.').to_string();
        let s = sum_for_count_query(&tests, &folder);
        let b = tests
            .iter()
            .filter(|(_, fk, _)| {
                fk == p.as_str() || fk.starts_with(&format!("{}.", p))
            })
            .count();
        (p, s, b)
    } else {
        let p = resolve_short_segment_to_prefix(&tests, &folder)
            .with_context(|| {
                format!(
                    "No tree path has a top-level segment matching {:?} (try a full VSTest prefix like Tmly.Test.Imports)",
                    folder
                )
            })?;
        let s = sum_for_count_query(&tests, &folder);
        let b = tests
            .iter()
            .filter(|(tree, _, _)| tree_under_prefix(tree, &p))
            .count();
        (p, s, b)
    };
    println!("Resolved prefix: {}", ptrim);
    println!("Total list-line count (incl. TestCase rows): {}", sum);
    println!("Distinct method entries: {}", buckets);
    Ok(())
}
