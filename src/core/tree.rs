use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct TreeNode {
    pub label: String,
    /// filter_key: the display-name base used in `FullyQualifiedName~` filters.
    /// None for folder / class (non-leaf) nodes.
    pub fqn: Option<String>,
    pub is_selected: bool,
    pub is_expanded: bool,
    pub depth: usize,
    pub parent_idx: Option<usize>,
    pub is_leaf: bool,
}

struct NodeBuilder {
    children: BTreeMap<String, NodeBuilder>,
    /// (leaf_label, filter_key)
    leaves: Vec<(String, String)>,
}

impl NodeBuilder {
    fn new() -> Self {
        NodeBuilder { children: BTreeMap::new(), leaves: Vec::new() }
    }
}

fn flatten_node(node: &NodeBuilder, flat: &mut Vec<TreeNode>, depth: usize, parent_idx: Option<usize>, prefix: &str) {
    // Folders / class nodes first (alphabetically, from BTreeMap)
    for (label, child) in &node.children {
        let idx = flat.len();
        let new_prefix = if prefix.is_empty() { label.clone() } else { format!("{}.{}", prefix, label) };
        flat.push(TreeNode {
            label: label.clone(),
            fqn: Some(new_prefix.clone()),
            is_selected: false,   // <- default unselected
            is_expanded: false,
            depth,
            parent_idx,
            is_leaf: false,
        });
        flatten_node(child, flat, depth + 1, Some(idx), &new_prefix);
    }

    // Then leaf tests
    for (label, filter_key) in &node.leaves {
        flat.push(TreeNode {
            label: label.clone(),
            fqn: Some(filter_key.clone()),
            is_selected: false,   // <- default unselected
            is_expanded: true,
            depth,
            parent_idx,
            is_leaf: true,
        });
    }
}

/// Build a flat, render-ready tree from enriched test entries.
///
/// `tests` is a slice of `(tree_fqn, filter_key)`:
///   - `tree_fqn`   dot-separated path used for the visual hierarchy
///                  e.g. `Import.ImportAdjustment` or
///                       `Import.DeleteBatch.ImportSameFile`
///   - `filter_key` the plain display-name base (no params) stored on each leaf
///                  e.g. `ImportAdjustment` or `ImportBillingContactAssignment.ShouldImport`
///
/// The tree is built recursively:
///   - Every dot-segment except the last -> non-leaf folder/class node
///   - The last segment                  -> leaf test node
///
/// Depth 0 = folder (pink), depth 1 = class (cyan), depth 2+ = test method.
pub fn build_flat_tree(tests: &[(String, String)]) -> Vec<TreeNode> {
    let mut root = NodeBuilder::new();

    for (tree_fqn, filter_key) in tests {
        let parts: Vec<&str> = tree_fqn.split('.').collect();
        let len = parts.len();
        if len == 0 { continue; }

        let mut current = &mut root;
        // Navigate (or create) intermediate nodes for all segments except the last
        for &part in &parts[..len - 1] {
            current = current.children.entry(part.to_string()).or_insert_with(NodeBuilder::new);
        }

        // Last segment = display label of the leaf
        let leaf_label = parts[len - 1].to_string();

        // Deduplicate by filter_key (parameterised variants share the same base name)
        if !current.leaves.iter().any(|(_, k)| k == filter_key) {
            current.leaves.push((leaf_label, filter_key.clone()));
        }
    }

    let mut flat = Vec::new();
    flatten_node(&root, &mut flat, 0, None, "");
    flat
}
