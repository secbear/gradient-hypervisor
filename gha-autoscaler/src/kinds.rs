use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub systems: Vec<String>,
    pub features: Vec<String>,
    pub max_jobs: u32,
    pub ram_mb: Option<u64>,
}
