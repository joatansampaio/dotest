use std::collections::HashSet;

use crate::core::tree::{sync_parents, TreeNode};

use super::config::{RunConfig, TestPreset};

pub(crate) struct PresetApplyResult {
    pub applied: usize,
    pub missing: usize,
}

pub(crate) fn collect_selected_tests(tree: &[TreeNode]) -> Vec<String> {
    tree.iter()
        .filter(|n| n.is_leaf && n.is_selected)
        .filter_map(|n| n.fqn.clone())
        .collect()
}

pub(crate) fn save_preset(
    run_config: &mut RunConfig,
    tree: &[TreeNode],
    name: &str,
    tag: Option<String>,
) -> Result<usize, String> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err("Preset name is required.".to_string());
    }
    if run_config
        .presets
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(trimmed_name))
    {
        return Err(format!("Preset '{trimmed_name}' already exists."));
    }

    let tests = collect_selected_tests(tree);
    if tests.is_empty() {
        return Err("Select at least one test before saving a preset.".to_string());
    }

    let tag = tag.and_then(|t| {
        let trimmed = t.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });

    run_config.presets.push(TestPreset {
        name: trimmed_name.to_string(),
        tag,
        tests,
    });
    run_config.presets.sort_by_key(|p| p.name.to_lowercase());
    Ok(run_config.presets.len())
}

pub(crate) fn apply_preset_selection(
    tree: &mut Vec<TreeNode>,
    preset: &TestPreset,
) -> PresetApplyResult {
    let wanted: HashSet<&str> = preset.tests.iter().map(|s| s.as_str()).collect();
    for node in tree.iter_mut() {
        node.is_selected = false;
    }

    let mut applied = 0usize;
    for node in tree.iter_mut().filter(|n| n.is_leaf) {
        let is_selected = node.fqn.as_deref().is_some_and(|f| wanted.contains(f));
        node.is_selected = is_selected;
        if is_selected {
            applied += 1;
        }
    }
    sync_parents(tree);

    PresetApplyResult {
        applied,
        missing: wanted.len().saturating_sub(applied),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(name: &str, selected: bool) -> TreeNode {
        TreeNode {
            label: name.to_string(),
            fqn: Some(name.to_string()),
            is_selected: selected,
            is_partial: false,
            is_expanded: true,
            depth: 0,
            parent_idx: None,
            is_leaf: true,
            test_count: 1,
            target_path: None,
        }
    }

    #[test]
    fn save_preset_requires_unique_name() {
        let mut config = RunConfig::default();
        config.presets.push(TestPreset {
            name: "Smoke".to_string(),
            tag: None,
            tests: vec!["A".to_string()],
        });
        let tree = vec![leaf("A", true)];

        let err = save_preset(&mut config, &tree, "smoke", None).unwrap_err();
        assert!(err.contains("already exists"));
    }

    #[test]
    fn apply_preset_counts_missing_tests() {
        let mut tree = vec![leaf("A", false), leaf("B", false)];
        let preset = TestPreset {
            name: "Sample".to_string(),
            tag: None,
            tests: vec!["A".to_string(), "Missing".to_string()],
        };

        let result = apply_preset_selection(&mut tree, &preset);
        assert_eq!(result.applied, 1);
        assert_eq!(result.missing, 1);
        assert!(tree[0].is_selected);
        assert!(!tree[1].is_selected);
    }
}
