use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Pull request states
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// Represents a jj revision with associated PR information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_state: Option<PrState>,
}

impl Revision {
    /// Create a new revision
    pub fn new(change_id: String, commit_id: String, description: String) -> Self {
        Self {
            change_id,
            commit_id,
            description,
            branch_name: None,
            pr_url: None,
            full_description: None,
            pr_number: None,
            pr_state: None,
        }
    }

    /// Return abbreviated change ID
    pub fn short_change_id(&self) -> &str {
        &self.change_id[..self.change_id.len().min(8)]
    }

    /// Extract PR number from URL
    pub fn extract_pr_number(&self) -> Option<u32> {
        self.pr_url.as_ref().and_then(|url| {
            url.rsplit('/')
                .next()
                .and_then(|num| num.parse::<u32>().ok())
        })
    }
}

/// PR information stored in state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrInfo {
    pub change_id: String,
    pub pr_number: u32,
    pub pr_url: String,
    pub branch_name: String,
    pub commit_id: String,
    pub description: String,
    pub last_seen: DateTime<Local>,
}

/// Information about closed PRs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClosedPrInfo {
    pub branch_name: String,
    pub pr_number: u32,
    pub closed_at: DateTime<Local>,
    pub reason: String,
}

/// Current version of the state file format
pub const STATE_VERSION: u32 = 2;

/// State persisted between runs - V2 format optimized for merge conflicts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// Version of the state file format
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Local>>,
    /// List of active PRs (instead of HashMap to reduce conflicts)
    #[serde(default)]
    pub prs: Vec<PrInfo>,
    /// List of closed PRs (instead of HashMap to reduce conflicts)
    #[serde(default)]
    pub closed_prs: Vec<ClosedPrInfo>,
    /// List of bookmarks (sorted for consistency)
    #[serde(default)]
    pub bookmarks: Vec<String>,
    /// Set of change IDs that have merged PRs (permanent)
    #[serde(default)]
    pub merged_pr_change_ids: HashSet<String>,
    /// Set of change IDs that have closed PRs (permanent)
    #[serde(default)]
    pub closed_pr_change_ids: HashSet<String>,

    // Legacy fields for backward compatibility (v1)
    #[serde(skip_serializing, default)]
    pub prs_map: HashMap<String, PrInfo>,
    #[serde(skip_serializing, default)]
    pub closed_prs_map: HashMap<String, ClosedPrInfo>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            last_run: None,
            prs: Vec::new(),
            closed_prs: Vec::new(),
            bookmarks: Vec::new(),
            merged_pr_change_ids: HashSet::new(),
            closed_pr_change_ids: HashSet::new(),
            prs_map: HashMap::new(),
            closed_prs_map: HashMap::new(),
        }
    }
}

impl State {
    /// Convert v1 HashMap format to v2 Vec format
    pub fn migrate_from_v1(&mut self) {
        // Migrate PRs from map to vec
        if !self.prs_map.is_empty() {
            self.prs = self
                .prs_map
                .iter()
                .map(|(change_id, info)| PrInfo {
                    change_id: change_id.clone(),
                    pr_number: info.pr_number,
                    pr_url: info.pr_url.clone(),
                    branch_name: info.branch_name.clone(),
                    commit_id: info.commit_id.clone(),
                    description: info.description.clone(),
                    last_seen: info.last_seen,
                })
                .collect();
            self.prs.sort_by(|a, b| a.change_id.cmp(&b.change_id));
            self.prs_map.clear();
        }

        // Migrate closed PRs from map to vec
        if !self.closed_prs_map.is_empty() {
            self.closed_prs = self
                .closed_prs_map
                .iter()
                .map(|(branch_name, info)| ClosedPrInfo {
                    branch_name: branch_name.clone(),
                    pr_number: info.pr_number,
                    closed_at: info.closed_at,
                    reason: info.reason.clone(),
                })
                .collect();
            self.closed_prs
                .sort_by(|a, b| a.branch_name.cmp(&b.branch_name));
            self.closed_prs_map.clear();
        }

        // Sort bookmarks for consistency
        self.bookmarks.sort();
        self.bookmarks.dedup();
    }

    /// Get PR info by change ID
    #[allow(dead_code)]
    pub fn get_pr(&self, change_id: &str) -> Option<&PrInfo> {
        self.prs.iter().find(|pr| pr.change_id == change_id)
    }

    /// Get closed PR info by branch name
    pub fn get_closed_pr(&self, branch_name: &str) -> Option<&ClosedPrInfo> {
        self.closed_prs
            .iter()
            .find(|pr| pr.branch_name == branch_name)
    }
}

fn default_version() -> u32 {
    STATE_VERSION
}

/// GitHub PR data from API
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubPr {
    pub number: u32,
    pub head_ref_name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "baseRefName")]
    pub base_ref_name: Option<String>,
    #[serde(default)]
    pub state: String,
}
