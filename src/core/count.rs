//! Summing discovered tests for `dotest count` and sanity-checks (aligned with UI totals).

use crate::core::discovery::DiscoveredTest;

/// Sum rows for `dotest count <query>`:
/// - **Short name** (no dots): match the **discovery tree path** (disk-first) by top-level folder segment.
/// - **Qualified prefix** (contains `.`, e.g. `Tmly.Test.Imports`): match the **VSTest filter** column.
pub fn sum_for_count_query(tests: &[DiscoveredTest], query: &str) -> usize {
    let q = query.trim().trim_end_matches('.');
    if q.is_empty() {
        return 0;
    }
    if q.contains('.') {
        tests
            .iter()
            .filter(|test| {
                test.filter_key == q || test.filter_key.starts_with(&format!("{}.", q))
            })
            .map(|test| test.test_count)
            .sum()
    } else {
        tests
            .iter()
            .filter(|test| tree_in_top_level_disk_folder(&test.tree_path, q))
            .map(|test| test.test_count)
            .sum()
    }
}

fn tree_in_top_level_disk_folder(tree: &str, folder: &str) -> bool {
    tree.split('.')
        .next()
        .map(|seg| seg.eq_ignore_ascii_case(folder))
        .unwrap_or(false)
}

pub fn tree_under_prefix(tree: &str, prefix: &str) -> bool {
    tree == prefix || tree.starts_with(&format!("{}.", prefix))
}

/// Resolve a short folder label (`Groups`, `Imports`) to the canonical first path segment from data.
pub fn resolve_short_segment_to_prefix(
    tests: &[DiscoveredTest],
    segment: &str,
) -> Option<String> {
    let want = segment.trim();
    if want.is_empty() {
        return None;
    }
    if want.contains('.') {
        return Some(want.to_string());
    }
    for test in tests {
        let first = test.tree_path.split('.').next()?;
        if first.eq_ignore_ascii_case(want) {
            return Some(first.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_under_prefix_boundary() {
        assert!(tree_under_prefix(
            "Tmly.Test.Groups.Foo.Bar",
            "Tmly.Test.Groups"
        ));
        assert!(!tree_under_prefix(
            "Tmly.Test.GroupsHelper.Foo",
            "Tmly.Test.Groups"
        ));
        assert!(tree_under_prefix("Tmly.Test.Groups", "Tmly.Test.Groups"));
    }

    #[test]
    fn resolve_segment_finds_top_level_folder() {
        let tests = vec![
            DiscoveredTest::new("Groups.OrgTreeTests.X", "Tmly.Test.Groups.OrgTreeTests.X", 1),
            DiscoveredTest::new("Imports.A.M", "Tmly.Test.Imports.A.M", 1),
        ];
        let p = resolve_short_segment_to_prefix(&tests, "Groups").unwrap();
        assert_eq!(p, "Groups");
    }

    #[test]
    fn sum_count_query_disk_vs_vstest() {
        let tests = vec![
            DiscoveredTest::new("Imports.M1", "Ns.Imports.C.M1", 3),
            DiscoveredTest::new("Imports.M2", "Ns.Imports.C.M2", 1),
            DiscoveredTest::new("Groups.G1", "Ns.Groups.G1", 10),
        ];
        assert_eq!(sum_for_count_query(&tests, "Imports"), 4);
        assert_eq!(sum_for_count_query(&tests, "Ns.Imports"), 4);
    }
}
