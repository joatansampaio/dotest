use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredTest {
    pub tree_path: String,
    pub filter_key: String,
    pub test_count: usize,
    #[serde(default)]
    pub target_path: Option<String>,
}

impl DiscoveredTest {
    pub fn new(
        tree_path: impl Into<String>,
        filter_key: impl Into<String>,
        test_count: usize,
    ) -> Self {
        Self {
            tree_path: tree_path.into(),
            filter_key: filter_key.into(),
            test_count,
            target_path: None,
        }
    }

    pub fn with_target(mut self, target_path: impl Into<String>) -> Self {
        self.target_path = Some(target_path.into());
        self
    }
}