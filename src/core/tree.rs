use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug)]
pub struct TreeNode {
    pub label: String,
    /// filter_key: the display-name base used in `FullyQualifiedName~` filters.
    /// None for folder / class (non-leaf) nodes.
    pub fqn: Option<String>,
    pub is_selected: bool,
    /// True when this non-leaf node has *some* but not *all* leaf descendants selected.
    /// Always false for leaf nodes.
    pub is_partial: bool,
    pub is_expanded: bool,
    pub depth: usize,
    pub parent_idx: Option<usize>,
    pub is_leaf: bool,
    /// Number of actual test instances this node represents.
    /// For leaves: count of parameterised variants (e.g. [TestCase] produces N).
    /// For non-leaves: always 0 (use helper fn to sum children).
    pub test_count: usize,
}

struct NodeBuilder {
    children: BTreeMap<String, NodeBuilder>,
    /// (leaf_label, filter_key, test_count)
    leaves: Vec<(String, String, usize)>,
}

impl NodeBuilder {
    fn new() -> Self {
        NodeBuilder {
            children: BTreeMap::new(),
            leaves: Vec::new(),
        }
    }
}

fn flatten_node(
    node: &NodeBuilder,
    flat: &mut Vec<TreeNode>,
    depth: usize,
    parent_idx: Option<usize>,
    prefix: &str,
) {
    // Folders / class nodes first (alphabetically, from BTreeMap)
    for (label, child) in &node.children {
        let idx = flat.len();
        let new_prefix = if prefix.is_empty() {
            label.clone()
        } else {
            format!("{}.{}", prefix, label)
        };
        flat.push(TreeNode {
            label: label.clone(),
            fqn: Some(new_prefix.clone()),
            is_selected: false,
            is_partial: false,
            is_expanded: false,
            depth,
            parent_idx,
            is_leaf: false,
            test_count: 0,
        });
        flatten_node(child, flat, depth + 1, Some(idx), &new_prefix);
    }

    // Then leaf tests
    for (label, filter_key, count) in &node.leaves {
        flat.push(TreeNode {
            label: label.clone(),
            fqn: Some(filter_key.clone()),
            is_selected: false,
            is_partial: false,
            is_expanded: true,
            depth,
            parent_idx,
            is_leaf: true,
            test_count: *count,
        });
    }
}

/// Build a flat, render-ready tree from enriched test entries.
///
/// `tests` is a slice of `(tree_fqn, filter_key, test_count)`:
///   - `tree_fqn`    dot-separated path used for the visual hierarchy
///   - `filter_key`  the plain display-name base (no params) stored on each leaf
///   - `test_count`  number of actual test instances (parameterised variants)
///
/// The tree is built recursively:
///   - Every dot-segment except the last -> non-leaf folder/class node
///   - The last segment                  -> leaf test node
///
/// Depth 0 = folder (pink), depth 1 = class (cyan), depth 2+ = test method.
pub fn build_flat_tree(tests: &[(String, String, usize)]) -> Vec<TreeNode> {
    let mut root = NodeBuilder::new();

    for (tree_fqn, filter_key, count) in tests {
        let parts: Vec<&str> = tree_fqn.split('.').collect();
        let len = parts.len();
        if len == 0 {
            continue;
        }

        let mut current = &mut root;
        // Navigate (or create) intermediate nodes for all segments except the last
        for &part in &parts[..len - 1] {
            current = current
                .children
                .entry(part.to_string())
                .or_insert_with(NodeBuilder::new);
        }

        // Last segment = display label of the leaf
        let leaf_label = parts[len - 1].to_string();

        // Deduplicate by filter_key (parameterised variants share the same base name)
        if !current.leaves.iter().any(|(_, k, _)| k == filter_key) {
            current
                .leaves
                .push((leaf_label, filter_key.clone(), *count));
        }
    }

    let mut flat = Vec::new();
    flatten_node(&root, &mut flat, 0, None, "");
    annotate_non_leaf_vstest_filters(&mut flat);
    flat
}

/// Replace non-leaf `fqn` (still the UI dot-path from flatten) with a VSTest filter prefix.
/// Uses leaf filter segment counts + tree depth so depth-0 nodes get namespace-only prefixes
fn annotate_non_leaf_vstest_filters(flat: &mut [TreeNode]) {
    for i in 0..flat.len() {
        if flat[i].is_leaf {
            continue;
        }
        let node_depth = flat[i].depth;
        let mut leaf_filters: Vec<String> = Vec::new();
        let mut leaf_depth: Option<usize> = None;
        let mut j = i + 1;
        while j < flat.len() && flat[j].depth > node_depth {
            if flat[j].is_leaf {
                if let Some(f) = flat[j].fqn.clone() {
                    if leaf_depth.is_none() {
                        leaf_depth = Some(flat[j].depth);
                    }
                    leaf_filters.push(f);
                }
            }
            j += 1;
        }
        if leaf_filters.is_empty() {
            continue;
        }
        let leaf_depth = leaf_depth.unwrap_or(0);
        let nseg = leaf_filters[0].split('.').count();
        let take_segments = nseg.saturating_sub(leaf_depth.saturating_sub(node_depth));
        let merged = if leaf_filters.len() == 1 {
            leaf_filters[0].clone()
        } else {
            multi_lcp_string(&leaf_filters)
        };
        let prefix = truncate_to_dot_segments(&merged, take_segments);
        flat[i].fqn = Some(prefix);
    }
}

fn multi_lcp_string(filters: &[String]) -> String {
    let mut acc = filters[0].as_str();
    for f in &filters[1..] {
        acc = common_prefix_bytes(acc, f);
    }
    acc.to_string()
}

fn truncate_to_dot_segments(s: &str, max_segments: usize) -> String {
    if max_segments == 0 {
        return String::new();
    }
    let parts: Vec<&str> = s.split('.').collect();
    let n = parts.len().min(max_segments);
    parts[..n].join(".")
}

/// Propagates selection state up the tree: a non-leaf is selected iff all its leaf descendants are selected.
pub fn sync_parents(tree: &mut Vec<TreeNode>) {
    for i in (0..tree.len()).rev() {
        if tree[i].is_leaf { continue; }
        let mut any = false;
        let mut all = true;
        let mut j = i + 1;
        while j < tree.len() && tree[j].depth > tree[i].depth {
            if tree[j].is_leaf {
                if tree[j].is_selected { any = true; } else { all = false; }
            }
            j += 1;
        }
        tree[i].is_selected = all;
        tree[i].is_partial = any && !all;
    }
}

/// Captures and restores the selection and expansion state of a tree across a rebuild.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TreeState {
    /// fqn -> is_selected for every leaf in the old tree.
    leaves: HashMap<String, bool>,
    /// fqns of non-leaf nodes that were fully selected (all children checked).
    selected_groups: Vec<String>,
    /// fqns of non-leaf nodes that were expanded.
    expanded: std::collections::HashSet<String>,
}

impl TreeState {
    pub fn capture(tree: &[TreeNode]) -> Self {
        let leaves = tree
            .iter()
            .filter(|n| n.is_leaf)
            .filter_map(|n| n.fqn.as_ref().map(|f| (f.clone(), n.is_selected)))
            .collect();
        let selected_groups = tree
            .iter()
            .filter(|n| !n.is_leaf && n.is_selected)
            .filter_map(|n| n.fqn.clone())
            .collect();
        let expanded = tree
            .iter()
            .filter(|n| !n.is_leaf && n.is_expanded)
            .filter_map(|n| n.fqn.clone())
            .collect();
        Self { leaves, selected_groups, expanded }
    }

    pub fn restore(&self, tree: &mut Vec<TreeNode>) {
        for node in tree.iter_mut() {
            if let Some(ref fqn) = node.fqn {
                if node.is_leaf {
                    if let Some(&was_selected) = self.leaves.get(fqn) {
                        node.is_selected = was_selected;
                    } else {
                        // New test: inherit from a selected ancestor group.
                        node.is_selected = self.selected_groups.iter().any(|prefix| {
                            fqn == prefix || fqn.starts_with(&format!("{}.", prefix))
                        });
                    }
                } else {
                    node.is_expanded = self.expanded.contains(fqn);
                }
            }
        }
        sync_parents(tree);
    }
}

fn common_prefix_bytes<'a>(a: &'a str, b: &str) -> &'a str {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let n = ab.len().min(bb.len());
    let mut i = 0;
    while i < n && ab[i] == bb[i] {
        i += 1;
    }
    let mut end = i;
    while end > 0 && !a.is_char_boundary(end) {
        end -= 1;
    }
    &a[..end]
}
