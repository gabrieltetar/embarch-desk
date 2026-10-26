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

/// Most entries read per Harvest account in one call (spec.md §4). A year
/// of daily entries fits; a longer range comes back cut and says so.
pub const ENTRIES_CAP: usize = 2000;

/// What `entries_between` read, and whether the cap cut it short.
pub struct Entries {
    pub entries: Vec<Value>,
    pub truncated: bool,
}

/// The user's own entries from `from` to `to` (YYYY-MM-DD, inclusive), as
/// `(account, project, task, notes, date, hours)` — what an agent reads back,
/// and the examples a project guess is made from. Pages on `next_page` up to
/// `ENTRIES_CAP` per account.
pub async fn entries_between(conns: &[Account], from: &str, to: &str) -> Result<Entries> {
    let mut out = vec![];
    let mut truncated = false;
    for (c, a) in accounts(conns).await? {
        let me = get(&c.token, Some(a.account_id), &format!("{API}/users/me")).await?;
        let uid = me["id"].as_u64().unwrap_or(0);
        let (rows, cut) = crate::connectors::take_capped(ENTRIES_CAP, |page: Option<u64>| {
            let (token, a) = (c.token.clone(), a.clone());
            async move {
                let page = page.unwrap_or(1);
                let v = get(
                    &token,
                    Some(a.account_id),
                    &format!("{API}/time_entries?user_id={uid}&from={from}&to={to}&per_page=100&page={page}"),
                )
                .await?;
                Ok(entry_page(&v, &a))
            }
        })
        .await?;
        out.extend(rows);
        truncated |= cut;
    }
    Ok(Entries { entries: out, truncated })
}

/// One page of `/time_entries` as the rows an agent reads, and the next page.
fn entry_page(v: &Value, a: &HarvestAccount) -> (Vec<Value>, Option<u64>) {
    let rows = v["time_entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            json!({
                "id": e["id"], "is_running": e["is_running"],
                "account_id": a.account_id, "account": a.name,
                "project_id": e["project"]["id"], "project": e["project"]["name"], "client": e["client"]["name"],
                "task_id": e["task"]["id"], "task": e["task"]["name"],
                "notes": e["notes"], "date": e["spent_date"], "hours": e["hours"],
            })
        })
        .collect();
    (rows, v["next_page"].as_u64())
}

pub async fn recent_entries(conns: &[Account], days: i64) -> Result<Entries> {
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

/// The Harvest icon on the entries Linear's own integration makes [measured
/// 2026-10-26, from one such entry's `external_reference`].
const LINEAR_ICON: &str = "https://proxy.harvestfiles.com/production_harvestapp_public/uploads/platform_icons/linear.app.png?1614522198";

/// An entry's link to its Linear issue (decision 63), shaped as Linear's own
/// Harvest integration writes one: the issue key as `id`, the workspace's URL
/// key as `group_id`, the issue's short permalink. Harvest's docs name only
/// `id`, `group_id`, `account_id` and `permalink` as parameters; `service`
/// and its icon ride along so the entry reads as the integration's do. None
/// for a URL that is not a Linear issue's.
pub fn linear_reference(key: &str, url: &str) -> Option<Value> {
    let rest = url.strip_prefix("https://linear.app/")?;
    let mut parts = rest.split('/');
    let workspace = parts.next().filter(|w| !w.is_empty())?;
    if parts.next() != Some("issue") || key.is_empty() {
        return None;
    }
    Some(json!({
        "id": key, "group_id": workspace, "account_id": Value::Null,
        "permalink": format!("https://linear.app/{workspace}/issue/{key}"),
        "service": "linear.app", "service_icon_url": LINEAR_ICON,
    }))
}

/// What `create_entry` posts: hours to the hundredth, and the issue's link
/// when the entry has one.
fn entry_body(project_id: u64, task_id: u64, date: &str, hours: f64, notes: &str, reference: Option<&Value>) -> Value {
    let mut body = json!({
        "project_id": project_id, "task_id": task_id, "spent_date": date,
        "hours": (hours * 100.0).round() / 100.0, "notes": notes,
    });
    if let Some(r) = reference {
        body["external_reference"] = r.clone();
    }
    body
}

#[allow(clippy::too_many_arguments)]
pub async fn create_entry(
    conns: &[Account],
    account_id: u64,
    project_id: u64,
    task_id: u64,
    date: &str,
    hours: f64,
    notes: &str,
    reference: Option<&Value>,
) -> Result<u64> {
    for (c, a) in accounts(conns).await? {
        if a.account_id != account_id {
            continue;
        }
        let resp = http()
            .post(format!("{API}/time_entries"))
            .bearer_auth(&c.token)
            .header("Harvest-Account-Id", account_id.to_string())
            .json(&entry_body(project_id, task_id, date, hours, notes, reference))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> HarvestAccount {
        HarvestAccount { connector: "c".into(), account_id: 7, name: "Acme".into() }
    }

    #[test]
    fn an_issue_entry_carries_its_linear_link_as_linear_writes_it() {
        let r = linear_reference("ABC-123", "https://linear.app/acme/issue/ABC-123/fix-the-widget").unwrap();
        assert_eq!(r["id"], "ABC-123");
        assert_eq!(r["group_id"], "acme");
        assert_eq!(r["permalink"], "https://linear.app/acme/issue/ABC-123");
        assert_eq!(r["service"], "linear.app");
        assert!(r["account_id"].is_null());
        let body = entry_body(1, 2, "2026-10-26", 1.234, "n", Some(&r));
        assert_eq!(body["external_reference"], r);
        assert_eq!(body["hours"], 1.23);
        assert!(entry_body(1, 2, "2026-10-26", 1.0, "n", None).get("external_reference").is_none(), "no issue, no link");
        for url in ["", "https://example.com/acme/issue/ABC-123", "https://linear.app/acme/project/x", "https://linear.app//issue/ABC-123"] {
            assert!(linear_reference("ABC-123", url).is_none(), "{url}");
        }
        assert!(linear_reference("", "https://linear.app/acme/issue/ABC-123").is_none());
    }

    #[tokio::test]
    async fn entries_from_three_pages_all_arrive_and_the_cap_flags_a_cut() {
        // Synthetic pages: 100, 100, 50 entries, next_page chained.
        let pages = |cap: usize| async move {
            crate::connectors::take_capped(cap, |p: Option<u64>| async move {
                let p = p.unwrap_or(1);
                let n = if p == 3 { 50 } else { 100 };
                let list: Vec<Value> = (0..n).map(|i| json!({ "id": p * 1000 + i, "hours": 1.0 })).collect();
                let v = json!({ "time_entries": list, "next_page": if p < 3 { json!(p + 1) } else { Value::Null } });
                Ok(entry_page(&v, &acct()))
            })
            .await
            .unwrap()
        };
        let (rows, cut) = pages(2000).await;
        assert_eq!(rows.len(), 250);
        assert!(!cut);
        assert_eq!(rows[0]["account"], "Acme");
        assert_eq!(rows[249]["id"], 3049);
        let (rows, cut) = pages(120).await;
        assert_eq!(rows.len(), 120);
        assert!(cut);
    }
}
