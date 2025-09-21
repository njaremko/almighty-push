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
        }
    }

    /// Return abbreviated change ID
    pub fn short_change_id(&self) -> &str {
        &self.change_id[..self.change_id.len().min(8)]
    }

    /// Check if revision has an associated PR
    pub fn has_pr(&self) -> bool {
        self.pr_url.is_some()
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrInfo {
    pub pr_number: u32,
    pub pr_url: String,
    pub branch_name: String,
    pub commit_id: String,
    pub description: String,
    pub last_seen: DateTime<Local>,
}

/// Information about closed PRs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedPrInfo {
    pub pr_number: u32,
    pub closed_at: DateTime<Local>,
    pub reason: String,
}

/// State persisted between runs
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Local>>,
    #[serde(default)]
    pub prs: HashMap<String, PrInfo>,
    #[serde(default)]
    pub closed_prs_map: HashMap<String, ClosedPrInfo>,
    #[serde(default)]
    pub bookmarks: HashSet<String>,
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
