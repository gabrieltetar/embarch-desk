//! Harvest v2 with a personal access token. One token covers every account
//! its user belongs to; each request names its account in
//! `Harvest-Account-Id`. The desk reads the user's project assignments and
//! recent entries (so Claude can guess a tile's project and task) and
//! writes exactly one thing: a time entry, when a tile gets its ✓.

use crate::connectors::{http, Account};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

const API: &str = "https://api.harvestapp.com/v2";

#[derive(Debug, Clone, Serialize)]
pub struct HarvestAccount {
    pub connector: String,
    pub account_id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub account_id: u64,
    pub account_name: String,
    pub project_id: u64,
    pub project_name: String,
    pub client_name: String,
    pub task_id: u64,
    pub task_name: String,
}

async fn get(token: &str, account_id: Option<u64>, url: &str) -> Result<Value> {
    let mut req = http().get(url).bearer_auth(token);
    if let Some(a) = account_id {
        req = req.header("Harvest-Account-Id", a.to_string());
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("harvest: HTTP {status}: {}", body.get("error_description").or(body.get("message")).unwrap_or(&body));
    }
    Ok(body)
}

pub async fn accounts(conns: &[Account]) -> Result<Vec<(Account, HarvestAccount)>> {
    let mut out = vec![];
    for c in conns {
        let v = get(&c.token, None, "https://id.getharvest.com/api/v2/accounts").await?;
        for a in v["accounts"].as_array().into_iter().flatten() {
            if a["product"].as_str() == Some("harvest") {
                out.push((
                    c.clone(),
                    HarvestAccount {
                        connector: c.label.clone(),
                        account_id: a["id"].as_u64().unwrap_or(0),
                        name: a["name"].as_str().unwrap_or("?").to_string(),
                    },
                ));
            }
        }
    }
    Ok(out)
}

/// Every project and task the user can log to, across every account.
pub async fn targets(conns: &[Account]) -> Result<Vec<Target>> {
    let mut out = vec![];
    for (c, a) in accounts(conns).await? {
        let mut page = 1;
        loop {
            let v = get(
                &c.token,
                Some(a.account_id),
                &format!("{API}/users/me/project_assignments?per_page=100&page={page}"),
            )
            .await?;
            for pa in v["project_assignments"].as_array().into_iter().flatten() {
                if pa["is_active"].as_bool() == Some(false) {
                    continue;
                }
                for ta in pa["task_assignments"].as_array().into_iter().flatten() {
                    if ta["is_active"].as_bool() == Some(false) {
                        continue;
                    }
                    out.push(Target {
                        account_id: a.account_id,
                        account_name: a.name.clone(),
                        project_id: pa["project"]["id"].as_u64().unwrap_or(0),
                        project_name: pa["project"]["name"].as_str().unwrap_or("?").into(),
                        client_name: pa["client"]["name"].as_str().unwrap_or("").into(),
                        task_id: ta["task"]["id"].as_u64().unwrap_or(0),
                        task_name: ta["task"]["name"].as_str().unwrap_or("?").into(),
                    });
                }
            }
            match v["next_page"].as_u64() {
                Some(n) if n > page => page = n,
                _ => break,
            }
        }
    }
    Ok(out)
}

/// The user's own entries from `from` to `to` (YYYY-MM-DD, inclusive), as
/// `(account, project, task, notes, date, hours)` — what an agent reads back,
/// and the examples a project guess is made from.
pub async fn entries_between(conns: &[Account], from: &str, to: &str) -> Result<Vec<Value>> {
    let mut out = vec![];
    for (c, a) in accounts(conns).await? {
        let me = get(&c.token, Some(a.account_id), &format!("{API}/users/me")).await?;
        let uid = me["id"].as_u64().unwrap_or(0);
        let v = get(
            &c.token,
            Some(a.account_id),
            &format!("{API}/time_entries?user_id={uid}&from={from}&to={to}&per_page=100"),
        )
        .await?;
        for e in v["time_entries"].as_array().into_iter().flatten() {
            out.push(json!({
                "id": e["id"], "is_running": e["is_running"],
                "account_id": a.account_id, "account": a.name,
                "project_id": e["project"]["id"], "project": e["project"]["name"], "client": e["client"]["name"],
                "task_id": e["task"]["id"], "task": e["task"]["name"],
                "notes": e["notes"], "date": e["spent_date"], "hours": e["hours"],
            }));
        }
    }
    Ok(out)
}

pub async fn recent_entries(conns: &[Account], days: i64) -> Result<Vec<Value>> {
    let today = chrono::Local::now().date_naive();
    let from = today - chrono::Duration::days(days);
    entries_between(conns, &from.to_string(), &today.to_string()).await
}

/// The connector token that reaches a Harvest account.
async fn token_for(conns: &[Account], account_id: u64) -> Result<String> {
    for (c, a) in accounts(conns).await? {
        if a.account_id == account_id {
            return Ok(c.token);
        }
    }
    bail!("no connected Harvest account has id {account_id}")
}

/// Change an entry's hours and/or note — the user's edit from the day
/// list, sent as they left it.
pub async fn update_entry(conns: &[Account], account_id: u64, entry_id: u64, hours: Option<f64>, notes: Option<&str>) -> Result<()> {
    let token = token_for(conns, account_id).await?;
    let mut body = serde_json::Map::new();
    if let Some(h) = hours {
        body.insert("hours".into(), json!((h * 100.0).round() / 100.0));
    }
    if let Some(n) = notes {
        body.insert("notes".into(), json!(n));
    }
    let resp = http()
        .patch(format!("{API}/time_entries/{entry_id}"))
        .bearer_auth(&token)
        .header("Harvest-Account-Id", account_id.to_string())
        .json(&Value::Object(body))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("harvest refused the edit: HTTP {status}: {}", resp.text().await.unwrap_or_default());
    }
    Ok(())
}

pub async fn delete_entry(conns: &[Account], account_id: u64, entry_id: u64) -> Result<()> {
    let token = token_for(conns, account_id).await?;
    let resp = http()
        .delete(format!("{API}/time_entries/{entry_id}"))
        .bearer_auth(&token)
        .header("Harvest-Account-Id", account_id.to_string())
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("harvest refused the delete: HTTP {status}: {}", resp.text().await.unwrap_or_default());
    }
    Ok(())
}

pub async fn create_entry(
    conns: &[Account],
    account_id: u64,
    project_id: u64,
    task_id: u64,
    date: &str,
    hours: f64,
    notes: &str,
) -> Result<u64> {
    for (c, a) in accounts(conns).await? {
        if a.account_id != account_id {
            continue;
        }
        let resp = http()
            .post(format!("{API}/time_entries"))
            .bearer_auth(&c.token)
            .header("Harvest-Account-Id", account_id.to_string())
            .json(&json!({
                "project_id": project_id, "task_id": task_id, "spent_date": date,
                "hours": (hours * 100.0).round() / 100.0, "notes": notes,
            }))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("harvest refused the entry: HTTP {status}: {body}");
        }
        return body["id"].as_u64().ok_or_else(|| anyhow!("harvest returned no entry id"));
    }
    bail!("no connected Harvest account has id {account_id}")
}
