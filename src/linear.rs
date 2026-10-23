//! Linear over GraphQL with a personal API key. A key reaches one
//! workspace, so an issue key is asked of each account in turn and the one
//! that answers is remembered on the issue.

use crate::connectors::{http, Account};
use crate::store::IssueRef;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

const URL: &str = "https://api.linear.app/graphql";

async fn gql(acct: &Account, query: &str, vars: Value) -> Result<Value> {
    let resp = http()
        .post(URL)
        // A personal API key goes bare; `Bearer` is for OAuth tokens.
        .header("Authorization", acct.token.as_str())
        .json(&json!({ "query": query, "variables": vars }))
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await?;
    if let Some(errs) = body.get("errors") {
        bail!("linear ({}): {}", acct.label, errs);
    }
    if !status.is_success() {
        bail!("linear ({}): HTTP {status}", acct.label);
    }
    body.get("data").cloned().ok_or_else(|| anyhow!("linear: no data"))
}

/// The workspace's name, which is what a connector check reports.
pub async fn whoami(acct: &Account) -> Result<String> {
    let d = gql(acct, "{ viewer { name organization { name } } }", json!({})).await?;
    Ok(format!(
        "{} @ {}",
        d["viewer"]["name"].as_str().unwrap_or("?"),
        d["viewer"]["organization"]["name"].as_str().unwrap_or("?")
    ))
}

fn issue_from(acct: &Account, v: &Value, now: i64) -> Option<IssueRef> {
    let i = v.as_object()?;
    Some(IssueRef {
        account: acct.label.clone(),
        key: i.get("identifier")?.as_str()?.to_string(),
        id: i.get("id")?.as_str()?.to_string(),
        url: i.get("url")?.as_str()?.to_string(),
        title: i.get("title")?.as_str()?.to_string(),
        state_id: v["state"]["id"].as_str().unwrap_or_default().to_string(),
        state_name: v["state"]["name"].as_str().unwrap_or_default().to_string(),
        state_type: v["state"]["type"].as_str().unwrap_or_default().to_string(),
        team_id: v["team"]["id"].as_str().unwrap_or_default().to_string(),
        refreshed_ms: now,
    })
}

const ISSUE_FIELDS: &str = "id identifier title url state { id name type } team { id key }";

/// Find an issue by key (`ABC-123`) in whichever connected workspace has it.
pub async fn find_issue(accounts: &[Account], key: &str, now: i64) -> Result<Option<IssueRef>> {
    let q = format!("query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}");
    let mut last_err = None;
    for acct in accounts {
        match gql(acct, &q, json!({ "id": key })).await {
            Ok(d) => {
                if let Some(i) = issue_from(acct, &d["issue"], now) {
                    return Ok(Some(i));
                }
            }
            // "Entity not found" is how a workspace says "not mine".
            Err(e) if e.to_string().to_lowercase().contains("not found") => {}
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

pub async fn refresh_issue(accounts: &[Account], issue: &IssueRef, now: i64) -> Result<Option<IssueRef>> {
    let Some(acct) = accounts.iter().find(|a| a.label == issue.account) else {
        return find_issue(accounts, &issue.key, now).await;
    };
    let q = format!("query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}");
    let d = gql(acct, &q, json!({ "id": issue.id })).await?;
    Ok(issue_from(acct, &d["issue"], now))
}

/// The team's workflow states, in board order, for the card's dropdown.
pub async fn states(accounts: &[Account], issue: &IssueRef) -> Result<Vec<Value>> {
    let acct = accounts
        .iter()
        .find(|a| a.label == issue.account)
        .ok_or_else(|| anyhow!("no Linear account {:?} connected", issue.account))?;
    let d = gql(
        acct,
        "query($id: String!) { team(id: $id) { states { nodes { id name type position } } } }",
        json!({ "id": issue.team_id }),
    )
    .await?;
    let mut nodes = d["team"]["states"]["nodes"].as_array().cloned().unwrap_or_default();
    nodes.sort_by(|a, b| {
        a["position"].as_f64().unwrap_or(0.0).total_cmp(&b["position"].as_f64().unwrap_or(0.0))
    });
    Ok(nodes)
}

/// A status move — the one Linear write v1 has, and it goes straight
/// through (proposal decision 15).
pub async fn set_state(accounts: &[Account], issue: &IssueRef, state_id: &str, now: i64) -> Result<IssueRef> {
    let acct = accounts
        .iter()
        .find(|a| a.label == issue.account)
        .ok_or_else(|| anyhow!("no Linear account {:?} connected", issue.account))?;
    let q = format!(
        "mutation($id: String!, $s: String!) {{ issueUpdate(id: $id, input: {{ stateId: $s }}) \
         {{ success issue {{ {ISSUE_FIELDS} }} }} }}"
    );
    let d = gql(acct, &q, json!({ "id": issue.id, "s": state_id })).await?;
    if d["issueUpdate"]["success"].as_bool() != Some(true) {
        bail!("Linear refused the status change");
    }
    issue_from(acct, &d["issueUpdate"]["issue"], now).ok_or_else(|| anyhow!("Linear returned no issue"))
}

/// The account a key's team belongs to: the one that knows the issue.
async fn account_for_key<'a>(accounts: &'a [Account], key: &str) -> Result<(&'a Account, Value)> {
    let q = "query($id: String!) { issue(id: $id) { id identifier title url description priorityLabel \
             createdAt updatedAt state { id name type } team { id key name } assignee { name } \
             project { name } labels { nodes { name } } \
             comments(first: 25) { nodes { body createdAt user { name } } } } }";
    let mut last_err = None;
    for acct in accounts {
        match gql(acct, q, json!({ "id": key })).await {
            Ok(d) if d["issue"].is_object() => return Ok((acct, d["issue"].clone())),
            Ok(_) => {}
            Err(e) if e.to_string().to_lowercase().contains("not found") => {}
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no connected Linear workspace has {key}")))
}

/// An issue as an agent reads it: description, assignee, labels and the
/// latest comments, oldest first.
pub async fn read_issue(accounts: &[Account], key: &str) -> Result<Value> {
    let (acct, i) = account_for_key(accounts, key).await?;
    let comments: Vec<Value> = i["comments"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| json!({ "author": c["user"]["name"], "at": c["createdAt"], "body": c["body"] }))
        .rev()
        .collect();
    Ok(json!({
        "workspace": acct.label,
        "key": i["identifier"], "title": i["title"], "url": i["url"],
        "state": i["state"]["name"], "priority": i["priorityLabel"],
        "team": i["team"]["key"], "project": i["project"]["name"], "assignee": i["assignee"]["name"],
        "labels": i["labels"]["nodes"].as_array().map(|l| l.iter().map(|x| x["name"].clone()).collect::<Vec<_>>()),
        "description": i["description"],
        "comments": comments,
        "updated": i["updatedAt"],
    }))
}

fn issue_rows(acct: &Account, nodes: &Value) -> Vec<Value> {
    nodes
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| {
            json!({
                "workspace": acct.label, "key": n["identifier"], "title": n["title"],
                "state": n["state"]["name"], "url": n["url"], "updated": n["updatedAt"],
            })
        })
        .collect()
}

/// Open issues assigned to the user, in every connected workspace.
pub async fn my_issues(accounts: &[Account]) -> Result<Vec<Value>> {
    let q = "{ viewer { assignedIssues(first: 50, orderBy: updatedAt, \
             filter: { state: { type: { nin: [\"completed\", \"canceled\"] } } }) \
             { nodes { identifier title url updatedAt state { name } } } } }";
    let mut out = vec![];
    for acct in accounts {
        let d = gql(acct, q, json!({})).await?;
        out.extend(issue_rows(acct, &d["viewer"]["assignedIssues"]["nodes"]));
    }
    Ok(out)
}

/// Issues whose title contains the words, or the one a key names.
pub async fn search(accounts: &[Account], query: &str) -> Result<Vec<Value>> {
    if let Some(key) = crate::store::issue_key_in(query).filter(|k| k.eq_ignore_ascii_case(query.trim())) {
        return Ok(match read_issue(accounts, &key).await {
            Ok(v) => vec![v],
            Err(_) => vec![],
        });
    }
    let q = "query($q: String!) { issues(first: 20, orderBy: updatedAt, \
             filter: { title: { containsIgnoreCase: $q } }) \
             { nodes { identifier title url updatedAt state { name } } } }";
    let mut out = vec![];
    for acct in accounts {
        let d = gql(acct, q, json!({ "q": query.trim() })).await?;
        out.extend(issue_rows(acct, &d["issues"]["nodes"]));
    }
    Ok(out)
}

/// A status move by the state's name, as an agent says it ("In Review").
pub async fn set_state_by_name(accounts: &[Account], key: &str, name: &str, now: i64) -> Result<IssueRef> {
    let issue = find_issue(accounts, key, now).await?.ok_or_else(|| anyhow!("no connected Linear workspace has {key}"))?;
    let all = states(accounts, &issue).await?;
    let want = name.trim().to_lowercase();
    let st = all
        .iter()
        .find(|s| s["name"].as_str().is_some_and(|n| n.to_lowercase() == want))
        .ok_or_else(|| {
            let names: Vec<&str> = all.iter().filter_map(|s| s["name"].as_str()).collect();
            anyhow!("{key}'s team has no state {name:?}; it has: {}", names.join(", "))
        })?;
    set_state(accounts, &issue, st["id"].as_str().unwrap_or_default(), now).await
}

/// Post an approved comment. Only the Desk tab's Send reaches this.
pub async fn post_comment(accounts: &[Account], key: &str, body: &str) -> Result<String> {
    let (acct, i) = account_for_key(accounts, key).await?;
    let d = gql(
        acct,
        "mutation($id: String!, $b: String!) { commentCreate(input: { issueId: $id, body: $b }) { success comment { url } } }",
        json!({ "id": i["id"], "b": body }),
    )
    .await?;
    if d["commentCreate"]["success"].as_bool() != Some(true) {
        bail!("Linear refused the comment");
    }
    Ok(d["commentCreate"]["comment"]["url"].as_str().unwrap_or_default().to_string())
}

/// Create an approved issue in the team with this key, in whichever
/// workspace has that team.
pub async fn create_issue(accounts: &[Account], team_key: &str, title: &str, description: &str) -> Result<(String, String)> {
    for acct in accounts {
        let d = gql(
            acct,
            "query($k: String!) { teams(filter: { key: { eqIgnoreCase: $k } }) { nodes { id key } } }",
            json!({ "k": team_key }),
        )
        .await?;
        let Some(team) = d["teams"]["nodes"].as_array().and_then(|n| n.first().cloned()) else { continue };
        let d = gql(
            acct,
            "mutation($t: String!, $title: String!, $d: String) { issueCreate(input: { teamId: $t, title: $title, description: $d }) \
             { success issue { identifier url } } }",
            json!({ "t": team["id"], "title": title, "d": description }),
        )
        .await?;
        if d["issueCreate"]["success"].as_bool() != Some(true) {
            bail!("Linear refused the issue");
        }
        let i = &d["issueCreate"]["issue"];
        return Ok((i["identifier"].as_str().unwrap_or_default().into(), i["url"].as_str().unwrap_or_default().into()));
    }
    bail!("no connected Linear workspace has a team {team_key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issue_reads_out_of_the_graphql_shape() {
        let acct = Account { label: "work".into(), token: "t".into() };
        let v = json!({
            "id": "uuid", "identifier": "ABC-123", "title": "Bootload", "url": "https://linear.app/x/issue/ABC-123",
            "state": {"id": "s1", "name": "In Progress", "type": "started"},
            "team": {"id": "t1", "key": "CHA"}
        });
        let i = issue_from(&acct, &v, 5).unwrap();
        assert_eq!((i.key.as_str(), i.state_name.as_str(), i.account.as_str()), ("ABC-123", "In Progress", "work"));
        assert!(issue_from(&acct, &Value::Null, 5).is_none());
    }
}
