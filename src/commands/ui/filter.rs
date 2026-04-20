use crate::core::tree::TreeNode;

pub(super) fn build_filter(tree: &[TreeNode]) -> Option<String> {
    let mut any_selected = false;
    let mut all_selected = true;
    for node in tree.iter().filter(|n| n.is_leaf) {
        if node.is_selected { any_selected = true; }
        else { all_selected = false; }
    }

    if !any_selected { return None; }
    if all_selected { return Some(String::new()); }

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

    let exclude_str = tree.iter()
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

pub(super) fn sync_parents(tree: &mut Vec<TreeNode>) {
    for i in (0..tree.len()).rev() {
        if tree[i].is_leaf { continue; }
        let mut all = true;
        let mut j = i + 1;
        while j < tree.len() && tree[j].depth > tree[i].depth {
            if tree[j].is_leaf && !tree[j].is_selected { all = false; break; }
            j += 1;
        }
        tree[i].is_selected = all;
    }
}
