//! GitHub: the PRs that carry a work item's issue key, and their state.
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

/// The repos the user can see, most recently pushed first — the desk
/// form's suggestions.
pub async fn user_repos(accounts: &[Account]) -> Result<Vec<String>> {
    let mut out = vec![];
    for acct in accounts {
        let v = get(acct, &format!("{API}/user/repos?per_page=100&sort=pushed&affiliation=owner,collaborator,organization_member")).await?;
        out.extend(v.as_array().into_iter().flatten().filter_map(|r| r["full_name"].as_str().map(str::to_string)));
    }
    out.dedup();
    Ok(out)
}

/// The search query for a key's PRs: within the desk's repos when it names
/// some, else anything the user is involved in.
pub fn pr_query(key: &str, repos: &[String]) -> String {
    if repos.is_empty() {
        format!("{key} type:pr involves:@me")
    } else {
        let scope: Vec<String> = repos.iter().map(|r| format!("repo:{r}")).collect();
        format!("{key} type:pr {}", scope.join(" "))
    }
}

/// PRs that name `key`, newest first.
pub async fn prs_for(accounts: &[Account], key: &str, repos: &[String]) -> Result<Vec<PrRef>> {
    let mut out = vec![];
    for acct in accounts {
        let q = pr_query(key, repos);
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
            });
        }
    }
    out.dedup_by(|a, b| a.url == b.url);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desks_repos_scope_the_pr_search() {
        assert_eq!(pr_query("ABC-1", &[]), "ABC-1 type:pr involves:@me");
        assert_eq!(pr_query("ABC-1", &["o/a".into(), "o/b".into()]), "ABC-1 type:pr repo:o/a repo:o/b");
    }

    #[test]
    fn a_neighbouring_key_is_not_a_match() {
        assert!(names_key("ABC-123: bootload", "", "ABC-123"));
        assert!(names_key("wip", "gabriel/abc-123-bootload", "ABC-123"));
        assert!(!names_key("ABC-1230 other", "", "ABC-123"));
        assert!(!names_key("XABC-123", "", "ABC-123"));
    }

}
