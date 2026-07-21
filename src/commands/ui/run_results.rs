//! Last-run leaf outcomes: live attribution from console lines, parent rollups,
//! and lightweight persistence under `bin/dotest/last_run.json`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::tree::TreeNode;

use super::failed_tests::filter_key_for_vstest;

pub(crate) const LAST_RUN_PATH: &str = "bin/dotest/last_run.json";
const LAST_RUN_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LeafStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LeafResult {
    pub status: LeafStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ParentCounts {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl ParentCounts {
    pub(crate) fn format_suffix(self) -> String {
        let mut parts = Vec::new();
        if self.passed > 0 {
            parts.push(format!("{}✓", self.passed));
        }
        if self.failed > 0 {
            parts.push(format!("{}✗", self.failed));
        }
        if self.skipped > 0 {
            parts.push(format!("{}⚠", self.skipped));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("  {}", parts.join(" "))
        }
    }
}

#[derive(Serialize, Deserialize)]
struct LastRunFile {
    version: u32,
    results: BTreeMap<String, LeafResult>,
}

#[derive(Default)]
struct PendingFailure {
    /// Key already resolved against the tree (leaf `fqn`), or raw name if unmatched yet.
    key: String,
    details: Vec<String>,
}

/// In-memory last-run state keyed by leaf `TreeNode.fqn`.
#[derive(Default)]
pub(crate) struct RunResultsState {
    pub results: HashMap<String, LeafResult>,
    /// Parent tree index → aggregated descendant leaf counts.
    pub parent_counts: HashMap<usize, ParentCounts>,
    pending_failure: Option<PendingFailure>,
}

impl RunResultsState {
    pub(crate) fn clear(&mut self) {
        self.results.clear();
        self.parent_counts.clear();
        self.pending_failure = None;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    pub(crate) fn load() -> Self {
        let Ok(s) = fs::read_to_string(LAST_RUN_PATH) else {
            return Self::default();
        };
        let Ok(file) = serde_json::from_str::<LastRunFile>(&s) else {
            return Self::default();
        };
        if file.version != LAST_RUN_VERSION {
            return Self::default();
        }
        Self {
            results: file.results.into_iter().collect(),
            parent_counts: HashMap::new(),
            pending_failure: None,
        }
    }

    pub(crate) fn save(&self) {
        let dir = Path::new("bin/dotest");
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let file = LastRunFile {
            version: LAST_RUN_VERSION,
            results: self
                .results
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        if let Ok(s) = serde_json::to_string(&file) {
            let _ = fs::write(LAST_RUN_PATH, s);
        }
    }

    pub(crate) fn delete_file() {
        let _ = fs::remove_file(LAST_RUN_PATH);
    }

    /// Drop keys that no longer exist as leaves (e.g. after rediscovery).
    pub(crate) fn prune_to_tree(&mut self, tree: &[TreeNode]) {
        let leaf_keys: std::collections::HashSet<&str> = tree
            .iter()
            .filter(|n| n.is_leaf)
            .filter_map(|n| n.fqn.as_deref())
            .collect();
        self.results.retain(|k, _| leaf_keys.contains(k.as_str()));
        self.recompute_rollups(tree);
    }

    pub(crate) fn recompute_rollups(&mut self, tree: &[TreeNode]) {
        self.parent_counts.clear();
        let fqn_to_idx: HashMap<&str, usize> = tree
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_leaf)
            .filter_map(|(i, n)| n.fqn.as_deref().map(|f| (f, i)))
            .collect();

        for (fqn, result) in &self.results {
            let Some(&leaf_idx) = fqn_to_idx.get(fqn.as_str()) else {
                continue;
            };
            let mut parent = tree[leaf_idx].parent_idx;
            while let Some(pi) = parent {
                let counts = self.parent_counts.entry(pi).or_default();
                match result.status {
                    LeafStatus::Passed => counts.passed += 1,
                    LeafStatus::Failed => counts.failed += 1,
                    LeafStatus::Skipped => counts.skipped += 1,
                }
                parent = tree[pi].parent_idx;
            }
        }
    }

    /// Close any open failure detail block and recompute rollups.
    pub(crate) fn finalize_pending(&mut self, tree: &[TreeNode]) {
        if let Some(pending) = self.pending_failure.take() {
            self.set_result(
                pending.key,
                LeafResult {
                    status: LeafStatus::Failed,
                    details: pending.details,
                },
            );
        }
        self.recompute_rollups(tree);
    }

    fn set_result(&mut self, key: String, result: LeafResult) {
        self.results.insert(key, result);
    }

    /// Ingest one stdout line. Call during regular (non-churn) runs for live tree updates.
    pub(crate) fn ingest_line(&mut self, line: &str, tree: &[TreeNode]) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(pending) = self.pending_failure.as_mut() {
                pending.details.push(line.to_string());
            }
            return;
        }

        if let Some((status, name)) = parse_status_result_line(trimmed) {
            // Close previous failure block first.
            if let Some(pending) = self.pending_failure.take() {
                self.set_result(
                    pending.key,
                    LeafResult {
                        status: LeafStatus::Failed,
                        details: pending.details,
                    },
                );
            }

            let Some(key) = resolve_leaf_key(tree, &name, &self.results) else {
                // Ambiguous or unknown name — don't invent a key (would leave leaves grey / wrong).
                return;
            };

            match status {
                LeafStatus::Failed => {
                    self.pending_failure = Some(PendingFailure {
                        key: key.clone(),
                        details: Vec::new(),
                    });
                    // Placeholder so the tree turns red immediately.
                    self.set_result(
                        key,
                        LeafResult {
                            status: LeafStatus::Failed,
                            details: Vec::new(),
                        },
                    );
                }
                LeafStatus::Passed | LeafStatus::Skipped => {
                    self.set_result(
                        key,
                        LeafResult {
                            status,
                            details: Vec::new(),
                        },
                    );
                }
            }
            self.recompute_rollups(tree);
            return;
        }

        let lower = trimmed.to_lowercase();
        if lower.starts_with("total tests:")
            || lower.starts_with("passed:")
            || lower.starts_with("failed:")
            || lower.starts_with("skipped:")
            || trimmed.starts_with('━')
            || trimmed.starts_with("Test Run Summary")
        {
            if let Some(pending) = self.pending_failure.take() {
                self.set_result(
                    pending.key,
                    LeafResult {
                        status: LeafStatus::Failed,
                        details: pending.details,
                    },
                );
                self.recompute_rollups(tree);
            }
            return;
        }

        if let Some(pending) = self.pending_failure.as_mut() {
            pending.details.push(line.to_string());
        }
    }

    pub(crate) fn get(&self, fqn: &str) -> Option<&LeafResult> {
        self.results.get(fqn)
    }

    /// Lines to show in the Results-mode output panel for a leaf.
    pub(crate) fn leaf_panel_lines(&self, fqn: &str) -> Vec<String> {
        let Some(result) = self.results.get(fqn) else {
            return Vec::new();
        };
        let header = match result.status {
            LeafStatus::Passed => format!("Passed {fqn}"),
            LeafStatus::Failed => format!("Failed {fqn}"),
            LeafStatus::Skipped => format!("Skipped {fqn}"),
        };
        if result.details.is_empty() {
            vec![header]
        } else {
            let mut lines = vec![header, String::new()];
            lines.extend(result.details.iter().cloned());
            lines
        }
    }
}

fn parse_status_result_line(trimmed: &str) -> Option<(LeafStatus, String)> {
    let (status, after) = if let Some(rest) = trimmed.strip_prefix("Passed ") {
        (LeafStatus::Passed, rest)
    } else if let Some(rest) = trimmed.strip_prefix("Failed ") {
        (LeafStatus::Failed, rest)
    } else if let Some(rest) = trimmed.strip_prefix("Skipped ") {
        (LeafStatus::Skipped, rest)
    } else if trimmed.starts_with('✓') {
        let rest = trimmed.trim_start_matches('✓').trim();
        let rest = rest.strip_prefix("Passed ").unwrap_or(rest);
        (LeafStatus::Passed, rest)
    } else if trimmed.starts_with('✗') {
        let rest = trimmed.trim_start_matches('✗').trim();
        let rest = rest.strip_prefix("Failed ").unwrap_or(rest);
        (LeafStatus::Failed, rest)
    } else if trimmed.starts_with('⚠') {
        let rest = trimmed.trim_start_matches('⚠').trim();
        let rest = rest.strip_prefix("Skipped ").unwrap_or(rest);
        (LeafStatus::Skipped, rest)
    } else {
        return None;
    };

    // Ignore churn iteration banners like "Iteration 3   ✓ Passed (1.2s)"
    if after.is_empty() || after.starts_with('(') {
        return None;
    }

    let name = after.split(" [").next().unwrap_or(after).trim();
    if name.is_empty() {
        return None;
    }
    Some((status, name.to_string()))
}

/// Map a reported test name from console/sidecar output to a leaf `fqn`.
///
/// Console loggers often emit only the short method name. When several leaves share
/// that name (different classes/folders):
/// - prefer an exact FQN match
/// - else a unique suffix match
/// - else the uniquely selected suffix match
/// - else the next selected suffix match that does not yet have a result (so running
///   several homonyms together still paints each leaf as lines arrive)
///
/// Never guess by "longest FQN" or loose `contains`.
fn resolve_leaf_key(
    tree: &[TreeNode],
    reported_name: &str,
    existing: &HashMap<String, LeafResult>,
) -> Option<String> {
    let key = filter_key_for_vstest(reported_name);
    if key.is_empty() {
        return None;
    }

    let leaves: Vec<&TreeNode> = tree.iter().filter(|n| n.is_leaf).collect();

    if let Some(exact) = leaves.iter().find(|n| n.fqn.as_deref() == Some(key.as_str())) {
        return exact.fqn.clone();
    }

    let suffix_matches: Vec<&TreeNode> = leaves
        .iter()
        .copied()
        .filter(|n| {
            n.fqn
                .as_deref()
                .is_some_and(|fqn| fqn == key || fqn.ends_with(&format!(".{key}")))
        })
        .collect();

    match suffix_matches.as_slice() {
        [only] => only.fqn.clone(),
        [] => {
            let label_matches: Vec<&TreeNode> = leaves
                .iter()
                .copied()
                .filter(|n| n.label == key)
                .collect();
            match label_matches.as_slice() {
                [only] => only.fqn.clone(),
                many if !many.is_empty() => pick_among_homonyms(many, existing),
                _ => None,
            }
        }
        many => pick_among_homonyms(many, existing),
    }
}

/// Disambiguate leaves that share a short reported name.
fn pick_among_homonyms(
    candidates: &[&TreeNode],
    existing: &HashMap<String, LeafResult>,
) -> Option<String> {
    let selected: Vec<&TreeNode> = candidates
        .iter()
        .copied()
        .filter(|n| n.is_selected)
        .collect();

    match selected.as_slice() {
        [only] => only.fqn.clone(),
        [] => None,
        many => {
            // Running several selected homonyms: bind each result line to the next
            // selected leaf that still lacks an outcome.
            many
                .iter()
                .find(|n| {
                    n.fqn
                        .as_deref()
                        .is_some_and(|fqn| !existing.contains_key(fqn))
                })
                .or_else(|| many.first())
                .and_then(|n| n.fqn.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(fqn: &str, selected: bool) -> TreeNode {
        TreeNode {
            label: fqn.rsplit('.').next().unwrap_or(fqn).to_string(),
            fqn: Some(fqn.to_string()),
            is_selected: selected,
            is_partial: false,
            is_expanded: true,
            depth: 2,
            parent_idx: None,
            is_leaf: true,
            test_count: 1,
            target_path: None,
        }
    }

    #[test]
    fn parse_passed_failed_lines() {
        let (s, n) = parse_status_result_line("Passed Namespace.Class.Method [12 ms]").unwrap();
        assert_eq!(s, LeafStatus::Passed);
        assert_eq!(n, "Namespace.Class.Method");

        let (s, n) = parse_status_result_line("Failed Foo.Bar [1 ms]").unwrap();
        assert_eq!(s, LeafStatus::Failed);
        assert_eq!(n, "Foo.Bar");
    }

    #[test]
    fn parent_counts_suffix_omits_zeros() {
        let c = ParentCounts {
            passed: 3,
            failed: 1,
            skipped: 0,
        };
        assert_eq!(c.format_suffix(), "  3✓ 1✗");
    }

    #[test]
    fn resolve_leaf_key_prefers_selected_when_short_names_collide() {
        // Same method name under Groups vs Placements — console often emits only the short name.
        // Longest-FQN guessing used to paint Placements when Groups was the one selected/run.
        let groups =
            "Tmly.Test.Groups.GroupQueryHandlerTest.CanLimitScopeUnderTmlyGroupId";
        let placements =
            "Tmly.Test.Placements.PlacementQueryHandlerTests.CanLimitScopeUnderTmlyGroupId";
        let tree = vec![
            leaf(groups, true),
            leaf(placements, false),
        ];
        let empty = HashMap::new();

        assert_eq!(
            resolve_leaf_key(&tree, "CanLimitScopeUnderTmlyGroupId", &empty).as_deref(),
            Some(groups)
        );
        assert_eq!(
            resolve_leaf_key(&tree, groups, &empty).as_deref(),
            Some(groups)
        );
    }

    #[test]
    fn ingest_line_attributes_short_name_to_selected_homonym() {
        let groups =
            "Tmly.Test.Groups.GroupQueryHandlerTest.CanLimitScopeUnderTmlyGroupId";
        let placements =
            "Tmly.Test.Placements.PlacementQueryHandlerTests.CanLimitScopeUnderTmlyGroupId";
        let tree = vec![
            leaf(groups, true),
            leaf(placements, false),
        ];

        let mut state = RunResultsState::default();
        state.ingest_line("Passed CanLimitScopeUnderTmlyGroupId [5 ms]", &tree);

        assert_eq!(
            state.get(groups).map(|r| r.status),
            Some(LeafStatus::Passed)
        );
        assert!(state.get(placements).is_none());
    }

    #[test]
    fn ingest_line_attributes_each_homonym_when_both_selected() {
        let groups =
            "Tmly.Test.Groups.GroupQueryHandlerTest.CanLimitScopeUnderTmlyGroupId";
        let placements =
            "Tmly.Test.Placements.PlacementQueryHandlerTests.CanLimitScopeUnderTmlyGroupId";
        let tree = vec![
            leaf(groups, true),
            leaf(placements, true),
        ];

        let mut state = RunResultsState::default();
        state.ingest_line("Passed CanLimitScopeUnderTmlyGroupId [5 ms]", &tree);
        state.ingest_line("Passed CanLimitScopeUnderTmlyGroupId [7 ms]", &tree);

        assert_eq!(
            state.get(groups).map(|r| r.status),
            Some(LeafStatus::Passed)
        );
        assert_eq!(
            state.get(placements).map(|r| r.status),
            Some(LeafStatus::Passed)
        );
    }

    #[test]
    fn resolve_leaf_key_unique_suffix_without_selection() {
        let tree = vec![
            leaf("Ns.A.Foo", false),
            leaf("Ns.B.Bar", false),
        ];
        let empty = HashMap::new();
        assert_eq!(
            resolve_leaf_key(&tree, "Foo", &empty).as_deref(),
            Some("Ns.A.Foo")
        );
    }
}
