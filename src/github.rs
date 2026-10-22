//! GitHub: the PRs that carry a work item's issue key, with their CI state.
//! Read-only in v1 (proposal decision 14).

use crate::connectors::{http, Account};
use crate::store::PrRef;
use anyhow::{bail, Result};
use serde_json::Value;

const API: &str = "https://api.github.com";

async fn get(acct: &Account, url: &str) -> Result<Value> {
    let resp = http()
        .get(url)
        .bearer_auth(&acct.token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("github: HTTP {status}: {}", body["message"]);
    }
    Ok(body)
}

pub async fn whoami(acct: &Account) -> Result<String> {
    Ok(get(acct, &format!("{API}/user")).await?["login"].as_str().unwrap_or("?").to_string())
}

/// Does this PR really name the key, or did search match a neighbour
/// (`ABC-12` inside `ABC-123`)? Checked against title and branch.
pub fn names_key(title: &str, branch: &str, key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [title, branch].iter().any(|t| {
        let up = t.to_ascii_uppercase();
        up.match_indices(&key).any(|(i, _)| {
            let after = up.as_bytes().get(i + key.len());
            let before = i.checked_sub(1).and_then(|j| up.as_bytes().get(j));
            !after.is_some_and(|c| c.is_ascii_alphanumeric()) && !before.is_some_and(|c| c.is_ascii_alphanumeric())
        })
    })
}

fn summarize_checks(runs: &Value, combined: &Value) -> String {
    let mut any = false;
    let mut pending = false;
    for r in runs["check_runs"].as_array().into_iter().flatten() {
        any = true;
        match (r["status"].as_str(), r["conclusion"].as_str()) {
            (Some("completed"), Some("success" | "neutral" | "skipped")) => {}
            (Some("completed"), _) => return "failure".into(),
            _ => pending = true,
        }
    }
    match combined["state"].as_str() {
        Some("failure" | "error") => return "failure".into(),
        Some("pending") if combined["total_count"].as_u64().unwrap_or(0) > 0 => pending = true,
        Some("success") => any = true,
        _ => {}
    }
    if pending {
        "pending".into()
    } else if any {
        "success".into()
    } else {
        "none".into()
    }
}

/// PRs involving the user that name `key`, newest first.
pub async fn prs_for(accounts: &[Account], key: &str) -> Result<Vec<PrRef>> {
    let mut out = vec![];
    for acct in accounts {
        let q = format!("{key} type:pr involves:@me");
        let url = format!("{API}/search/issues?per_page=10&sort=updated&q={}", crate::uri::encode(&q));
        let found = get(acct, &url).await?;
        for it in found["items"].as_array().into_iter().flatten() {
            let Some(pr_url) = it["pull_request"]["url"].as_str() else { continue };
            let pr = get(acct, pr_url).await?;
            let title = pr["title"].as_str().unwrap_or_default();
            let branch = pr["head"]["ref"].as_str().unwrap_or_default();
            if !names_key(title, branch, key) {
                continue;
            }
            let repo = pr["base"]["repo"]["full_name"].as_str().unwrap_or_default().to_string();
            let sha = pr["head"]["sha"].as_str().unwrap_or_default();
            let ci = if sha.is_empty() {
                "none".to_string()
            } else {
                let runs = get(acct, &format!("{API}/repos/{repo}/commits/{sha}/check-runs?per_page=100"))
                    .await
                    .unwrap_or(Value::Null);
                let combined = get(acct, &format!("{API}/repos/{repo}/commits/{sha}/status"))
                    .await
                    .unwrap_or(Value::Null);
                summarize_checks(&runs, &combined)
            };
            let state = if pr["merged"].as_bool() == Some(true) {
                "merged"
            } else {
                pr["state"].as_str().unwrap_or("open")
            };
            out.push(PrRef {
                repo,
                number: pr["number"].as_u64().unwrap_or(0),
                title: title.to_string(),
                url: pr["html_url"].as_str().unwrap_or_default().to_string(),
                state: state.to_string(),
                draft: pr["draft"].as_bool().unwrap_or(false),
                ci,
            });
        }
    }
    out.dedup_by(|a, b| a.url == b.url);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_neighbouring_key_is_not_a_match() {
        assert!(names_key("ABC-123: bootload", "", "ABC-123"));
        assert!(names_key("wip", "gabriel/abc-123-bootload", "ABC-123"));
        assert!(!names_key("ABC-1230 other", "", "ABC-123"));
        assert!(!names_key("XABC-123", "", "ABC-123"));
    }

    #[test]
    fn ci_is_failure_if_anything_failed_and_pending_while_anything_runs() {
        let ok = json!({"check_runs": [{"status": "completed", "conclusion": "success"}]});
        let bad = json!({"check_runs": [{"status": "completed", "conclusion": "success"}, {"status": "completed", "conclusion": "failure"}]});
        let running = json!({"check_runs": [{"status": "in_progress", "conclusion": null}]});
        assert_eq!(summarize_checks(&ok, &Value::Null), "success");
        assert_eq!(summarize_checks(&bad, &Value::Null), "failure");
        assert_eq!(summarize_checks(&running, &Value::Null), "pending");
        assert_eq!(summarize_checks(&Value::Null, &json!({"state": "pending", "total_count": 0})), "none");
    }
}
