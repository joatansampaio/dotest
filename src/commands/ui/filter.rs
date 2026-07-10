use super::failed_tests::build_filter_for_display_names;
use crate::core::tree::TreeNode;

#[derive(Clone, Debug)]
pub(super) struct SelectedRunRequest {
    pub filter: String,
    pub sidecar_filter: String,
    pub target_path: Option<String>,
    pub test_names: Vec<String>,
}

pub(super) fn build_filter(tree: &[TreeNode]) -> Option<String> {
    let mut any_selected = false;
    let mut all_selected = true;
    for node in tree.iter().filter(|n| n.is_leaf) {
        if node.is_selected {
            any_selected = true;
        } else {
            all_selected = false;
        }
    }

    if !any_selected {
        return None;
    }
    if all_selected {
        return Some(String::new());
    }

    let mut include_nodes = Vec::new();
    for node in tree.iter() {
        if node.is_selected {
            let parent_is_selected = node.parent_idx.map_or(false, |pid| tree[pid].is_selected);
            if !parent_is_selected {
                if let Some(fqn) = node.fqn.as_deref() {
                    let pat = if node.is_leaf {
                        fqn.to_string()
                    } else if fqn.ends_with('.') {
                        fqn.to_string()
                    } else {
                        format!("{}.", fqn)
                    };
                    include_nodes.push(format!("FullyQualifiedName~{}", pat));
                }
            }
        }
    }
    let include_str = include_nodes.join("|");

    let exclude_str = tree
        .iter()
        .filter(|n| n.is_leaf && !n.is_selected)
        .filter_map(|n| n.fqn.as_deref())
        .map(|t| format!("FullyQualifiedName!~{}", t))
        .collect::<Vec<_>>()
        .join("&");

    if !exclude_str.is_empty() && exclude_str.len() < include_str.len() {
        Some(exclude_str)
    } else {
        Some(include_str)
    }
}

pub(super) fn build_selected_run_request(tree: &[TreeNode]) -> Option<SelectedRunRequest> {
    let filter = build_filter(tree)?;

    let mut test_names = Vec::new();
    let mut shared_target: Option<&str> = None;
    let mut single_target = true;

    for node in tree.iter().filter(|n| n.is_leaf && n.is_selected) {
        if let Some(fqn) = node.fqn.as_ref() {
            test_names.push(fqn.clone());
        }

        match node.target_path.as_deref() {
            Some(target) if single_target => {
                if let Some(existing) = shared_target {
                    if existing != target {
                        single_target = false;
                    }
                } else {
                    shared_target = Some(target);
                }
            }
            None => single_target = false,
            _ => {}
        }
    }

    if test_names.is_empty() {
        return None;
    }

    test_names.sort();
    test_names.dedup();
    let sidecar_filter = build_filter_for_display_names(&test_names);

    Some(SelectedRunRequest {
        filter,
        sidecar_filter,
        target_path: if single_target {
            shared_target.map(str::to_string)
        } else {
            None
        },
        test_names,
    })
}

#[cfg(test)]
mod tests {
    use super::build_selected_run_request;
    use crate::core::tree::TreeNode;

    fn leaf(fqn: &str, target_path: Option<&str>) -> TreeNode {
        TreeNode {
            label: fqn.rsplit('.').next().unwrap_or(fqn).to_string(),
            fqn: Some(fqn.to_string()),
            is_selected: true,
            is_partial: false,
            is_expanded: true,
            depth: 0,
            parent_idx: None,
            is_leaf: true,
            test_count: 1,
            target_path: target_path.map(str::to_string),
        }
    }

    #[test]
    fn selected_run_request_keeps_shared_target_path() {
        let tree = vec![
            leaf(
                "AppCore.Tests.WidgetTests.Renders",
                Some("smoke/root-app-tests/tests/AppCore.Tests/AppCore.Tests.csproj"),
            ),
            leaf(
                "AppCore.Tests.WidgetTests.Updates",
                Some("smoke/root-app-tests/tests/AppCore.Tests/AppCore.Tests.csproj"),
            ),
        ];

        let request = build_selected_run_request(&tree).expect("request");

        assert_eq!(
            request.target_path.as_deref(),
            Some("smoke/root-app-tests/tests/AppCore.Tests/AppCore.Tests.csproj")
        );
        assert!(request.filter.is_empty());
        assert_eq!(
            request.sidecar_filter,
            "FullyQualifiedName~AppCore.Tests.WidgetTests.Renders|FullyQualifiedName~AppCore.Tests.WidgetTests.Updates"
        );
        assert_eq!(
            request.test_names,
            vec![
                "AppCore.Tests.WidgetTests.Renders".to_string(),
                "AppCore.Tests.WidgetTests.Updates".to_string(),
            ]
        );
    }

    #[test]
    fn selected_run_request_clears_target_path_for_mixed_projects() {
        let tree = vec![
            leaf(
                "MultiTestsA.UnitTest1.Test1",
                Some("smoke/multi/MultiTestsA/MultiTestsA.csproj"),
            ),
            leaf(
                "MultiTestsB.UnitTest1.Test1",
                Some("smoke/multi/MultiTestsB/MultiTestsB.csproj"),
            ),
        ];

        let request = build_selected_run_request(&tree).expect("request");

        assert_eq!(request.target_path, None);
        assert!(request.filter.is_empty());
        assert_eq!(
            request.sidecar_filter,
            "FullyQualifiedName~MultiTestsA.UnitTest1.Test1|FullyQualifiedName~MultiTestsB.UnitTest1.Test1"
        );
        assert_eq!(
            request.test_names,
            vec![
                "MultiTestsA.UnitTest1.Test1".to_string(),
                "MultiTestsB.UnitTest1.Test1".to_string(),
            ]
        );
    }
}
