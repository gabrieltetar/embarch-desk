//! Linear over GraphQL with a personal API key. A key reaches one
//! workspace, so an issue key is asked of each account in turn and the one
//! that answers is remembered on the issue. An account signed in through
//! Linear's MCP server instead is answered by `linear_mcp`, in the same
//! shapes (decision 68).

use crate::connectors::{http, Account};
use crate::linear_mcp;
use crate::store::{IssueRef, LinearBinding};
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
    if acct.mcp.is_some() {
        return linear_mcp::whoami(acct).await;
    }
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
        project_id: v["project"]["id"].as_str().unwrap_or_default().to_string(),
        project_name: v["project"]["name"].as_str().unwrap_or_default().to_string(),
        branch_name: v["branchName"].as_str().unwrap_or_default().to_string(),
        refreshed_ms: now,
    })
}

const ISSUE_FIELDS: &str = "id identifier title url branchName state { id name type } team { id key } project { id name }";

/// Whether a GraphQL error is "Entity not found", which is how a workspace
/// says "not mine".
fn not_found(e: &anyhow::Error) -> bool {
    e.to_string().to_lowercase().contains("not found")
}

/// One workspace's issue by key or id; `None` when it has no such issue.
async fn issue_in(acct: &Account, id: &str, now: i64) -> Result<Option<IssueRef>> {
    if acct.mcp.is_some() {
        return linear_mcp::issue_ref(acct, id, now).await;
    }
    let q = format!("query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}");
    match gql(acct, &q, json!({ "id": id })).await {
        Ok(d) => Ok(issue_from(acct, &d["issue"], now)),
        Err(e) if not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Find an issue by key (`ABC-123`) in whichever connected workspace has it.
pub async fn find_issue(accounts: &[Account], key: &str, now: i64) -> Result<Option<IssueRef>> {
    let mut last_err = None;
    for acct in accounts {
        match issue_in(acct, key, now).await {
            Ok(Some(i)) => return Ok(Some(i)),
            Ok(None) => {}
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
    issue_in(acct, &issue.id, now).await
}

/// The team's workflow states, in board order, for the card's dropdown.
pub async fn states(accounts: &[Account], issue: &IssueRef) -> Result<Vec<Value>> {
    let acct = accounts
        .iter()
        .find(|a| a.label == issue.account)
        .ok_or_else(|| anyhow!("no Linear account {:?} connected", issue.account))?;
    team_states(acct, &issue.team_id).await
}

/// One workspace's team's workflow states, in board order.
async fn team_states(acct: &Account, team_id: &str) -> Result<Vec<Value>> {
    if acct.mcp.is_some() {
        return linear_mcp::team_states(acct, team_id).await;
    }
    let d = gql(
        acct,
        "query($id: String!) { team(id: $id) { states { nodes { id name type position } } } }",
        json!({ "id": team_id }),
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
    if acct.mcp.is_some() {
        return linear_mcp::move_state(acct, issue, state_id, now).await;
    }
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

/// Asks each account in turn until one has the key: `per` answers `None`
/// for a workspace without it.
async fn first_with<'a, T, F, Fut>(accounts: &'a [Account], key: &str, mut per: F) -> Result<T>
where
    F: FnMut(&'a Account) -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>>>,
{
    let mut last_err = None;
    for acct in accounts {
        match per(acct).await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| NoWorkspaceHas(key.to_string()).into()))
}

/// No connected workspace has the issue key. A typed error so the server can
/// say which case it is ([`no_key_message`]); alone it reads as it always did.
#[derive(Debug)]
pub struct NoWorkspaceHas(pub String);

impl std::fmt::Display for NoWorkspaceHas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no connected Linear workspace has {}", self.0)
    }
}

impl std::error::Error for NoWorkspaceHas {}

/// Why no workspace has `key`, in words an agent can act on (EMB-9). The
/// key's team (its prefix) is looked up first among the desks other than the
/// caller's: bound to one of them, the message names that desk and its folder,
/// since a session works on its own desk (decision 77). Else, when the
/// connected workspaces' `teams` are known and lack it, it says no connected
/// workspace has that team, naming no desk. Else the team is here and the
/// issue is not.
pub fn no_key_message(key: &str, teams: Option<&[Value]>, desks: &[&crate::store::Desk], caller: Option<&str>) -> String {
    let prefix = key.rsplit_once('-').map_or(key, |(p, _)| p).to_ascii_uppercase();
    let bound = |d: &&&crate::store::Desk| d.linear.as_ref().is_some_and(|l| l.team_key.eq_ignore_ascii_case(&prefix));
    if let Some(d) = desks.iter().filter(|d| Some(d.id.as_str()) != caller).find(bound) {
        let folder = d.firmware_repos.first().map_or_else(
            || "it has no folder yet; add one on the Desk tab".to_string(),
            |f| format!("open a session in {f}"),
        );
        return format!("{key} is in team {prefix}, which belongs to desk {:?}, not this session's; {folder}", d.name);
    }
    let known = teams.is_some_and(|t| t.iter().any(|t| t["key"].as_str().is_some_and(|k| k.eq_ignore_ascii_case(&prefix))));
    match teams {
        Some(_) if !known => format!("no connected Linear workspace has a team {prefix}, so {key} is not a key here; check the key"),
        _ => format!("no connected Linear workspace has {key}"),
    }
}

/// Comments an issue read returns at once: the newest of them.
pub const COMMENTS_PAGE: usize = 25;
/// Most comments one issue read counts (decision 59); past it the total is a lower bound.
pub const COMMENTS_CAP: usize = 500;
/// Comments one request asks for, on either account type.
pub const COMMENTS_FETCH: usize = 50;

/// One workspace's issue as GraphQL gives it, for `read_issue`; `None` when
/// it has no such key.
async fn gql_detail(acct: &Account, key: &str, skip: usize) -> Result<Option<Value>> {
    let q = "query($id: String!) { issue(id: $id) { id identifier title url description priorityLabel \
             createdAt updatedAt state { id name type } team { id key name } assignee { name } \
             project { name } labels { nodes { name } } } }";
    let i = match gql(acct, q, json!({ "id": key })).await {
        Ok(d) if d["issue"].is_object() => d["issue"].clone(),
        Ok(_) => return Ok(None),
        Err(e) if not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    // Assumed, not measured: `comments` lists newest first, as `.rev()` below
    // and the MCP side's `list_comments` both take it.
    let (rows, cut) = crate::connectors::take_capped(COMMENTS_CAP, |after: Option<String>| async move {
        let q = "query($id: String!, $a: String) { issue(id: $id) { comments(first: 50, after: $a) { \
                 nodes { id body createdAt user { name } parent { id } } pageInfo { hasNextPage endCursor } } } }";
        let d = gql(acct, q, json!({ "id": key, "a": after })).await?;
        let c = &d["issue"]["comments"];
        let rows: Vec<Value> = c["nodes"].as_array().cloned().unwrap_or_default();
        let next = (c["pageInfo"]["hasNextPage"] == true).then(|| c["pageInfo"]["endCursor"].as_str().map(str::to_string)).flatten();
        Ok((rows, next))
    })
    .await?;
    Ok(Some(detail_from(acct, &i, &rows, cut, skip)))
}

/// An issue as an agent reads it: description, assignee, labels and one
/// window of its comments (`COMMENTS_PAGE`, the newest after `skip` newer
/// ones), oldest first, with how many there are in all.
pub async fn read_issue(accounts: &[Account], key: &str, skip: usize) -> Result<Value> {
    first_with(accounts, key, |acct| async move {
        if acct.mcp.is_some() {
            linear_mcp::issue_detail(acct, key, skip).await
        } else {
            gql_detail(acct, key, skip).await
        }
    })
    .await
}

/// The comment fields of an issue read. `all` is every comment read, newest
/// first, `cut` whether the cap stopped the reading. The window skips the
/// `skip` newest and keeps `COMMENTS_PAGE`, returned oldest first; the
/// answer carries `comments_total` (a lower bound when `comments_capped`),
/// `comments_truncated` and a `comments_note` when some are not returned.
pub fn comment_fields(all: &[Value], cut: bool, skip: usize) -> Value {
    let total = all.len();
    let start = skip.min(total);
    let end = (start + COMMENTS_PAGE).min(total);
    let window: Vec<Value> = all[start..end].iter().rev().cloned().collect();
    let truncated = start > 0 || end < total || cut;
    let mut v = json!({ "comments": window, "comments_total": total, "comments_truncated": truncated, "comments_capped": cut });
    if truncated {
        let older = if cut { format!("at least {}", total - end) } else { (total - end).to_string() };
        v["comments_note"] = json!(format!(
            "Returned {} of {}{} comments: the newest after skipping {start}. {older} older and {start} newer ones are not returned; \
             read the older ones with `skip` set to {end}.",
            window.len(), total, if cut { "+" } else { "" }
        ));
    }
    v
}

fn detail_from(acct: &Account, i: &Value, rows: &[Value], cut: bool, skip: usize) -> Value {
    let comments: Vec<Value> = rows
        .iter()
        .map(|c| json!({ "id": c["id"], "parent": c["parent"]["id"], "author": c["user"]["name"], "at": c["createdAt"], "body": c["body"] }))
        .collect();
    let mut v = json!({
        "workspace": acct.label,
        "key": i["identifier"], "title": i["title"], "url": i["url"],
        "state": i["state"]["name"], "priority": i["priorityLabel"],
        "team": i["team"]["key"], "project": i["project"]["name"], "assignee": i["assignee"]["name"],
        "labels": i["labels"]["nodes"].as_array().map(|l| l.iter().map(|x| x["name"].clone()).collect::<Vec<_>>()),
        "description": i["description"],
        "updated": i["updatedAt"],
    });
    merge(&mut v, comment_fields(&comments, cut, skip));
    v
}

/// Copy `from`'s fields into `into`.
pub fn merge(into: &mut Value, from: Value) {
    if let (Some(a), Some(b)) = (into.as_object_mut(), from.as_object()) {
        a.extend(b.clone());
    }
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
                "project": n["project"]["name"],
            })
        })
        .collect()
}

/// The accounts a scoped call asks: its desk's one, or every one.
fn in_scope<'a>(accounts: &'a [Account], scope: Option<&LinearBinding>) -> Vec<&'a Account> {
    match scope {
        Some(b) => accounts.iter().filter(|a| a.label == b.account).collect(),
        None => accounts.iter().collect(),
    }
}

/// A desk bound to a project filters on it; one bound to a whole team, on the team.
pub(crate) fn scope_filter(b: &LinearBinding) -> (&'static str, &str) {
    if b.project_id.is_empty() {
        ("team", b.team_id.as_str())
    } else {
        ("project", b.project_id.as_str())
    }
}

const OPEN_STATES: &str = "state: { type: { nin: [\"completed\", \"canceled\"] } }";
const ROW_FIELDS: &str = "nodes { identifier title url updatedAt state { name } project { name } }";

/// Most open issues read per workspace (spec.md §4).
pub const MINE_CAP: usize = 500;

/// Open issues assigned to the user — in the caller's desk's Linear scope when
/// it has one, else in every connected workspace — and whether `MINE_CAP`
/// cut any workspace's list.
pub async fn my_issues(accounts: &[Account], scope: Option<&LinearBinding>) -> Result<(Vec<Value>, bool)> {
    open_rows(accounts, scope, true).await
}

/// Every open issue, whoever it is assigned to, in the same scope and under
/// the same cap (decision 71).
pub async fn open_issues(accounts: &[Account], scope: Option<&LinearBinding>) -> Result<(Vec<Value>, bool)> {
    open_rows(accounts, scope, false).await
}

/// The query for one page of open issues, and whose list it reads.
fn open_query(decl: &str, filter: &str, mine: bool) -> String {
    let args = format!("first: 50, after: $a, orderBy: updatedAt, filter: {{ {filter} }}");
    let body = format!("{ROW_FIELDS} pageInfo {{ hasNextPage endCursor }}");
    if mine {
        format!("query{decl} {{ viewer {{ assignedIssues({args}) {{ {body} }} }} }}")
    } else {
        format!("query{decl} {{ issues({args}) {{ {body} }} }}")
    }
}

async fn open_rows(accounts: &[Account], scope: Option<&LinearBinding>, mine: bool) -> Result<(Vec<Value>, bool)> {
    let mut out = vec![];
    let mut truncated = false;
    for acct in in_scope(accounts, scope) {
        if acct.mcp.is_some() {
            let (rows, cut) =
                crate::connectors::take_capped(MINE_CAP, |after: Option<String>| linear_mcp::open_page(acct, scope, mine, after)).await?;
            out.extend(rows);
            truncated |= cut;
            continue;
        }
        let (decl, filter, vars) = match scope {
            Some(b) => {
                let (field, id) = scope_filter(b);
                ("($p: ID, $a: String)", format!("{OPEN_STATES}, {field}: {{ id: {{ eq: $p }} }}"), json!({ "p": id }))
            }
            None => ("($a: String)", OPEN_STATES.to_string(), json!({})),
        };
        let q = open_query(decl, &filter, mine);
        let q = q.as_str();
        let vars = &vars;
        let (rows, cut) = crate::connectors::take_capped(MINE_CAP, |after: Option<String>| async move {
            let mut v = vars.clone();
            v["a"] = json!(after);
            let d = gql(acct, q, v).await?;
            Ok(issue_page(acct, if mine { &d["viewer"]["assignedIssues"] } else { &d["issues"] }))
        })
        .await?;
        out.extend(rows);
        truncated |= cut;
    }
    Ok((out, truncated))
}

/// One page of issues as rows, and the cursor of the next.
fn issue_page(acct: &Account, page: &Value) -> (Vec<Value>, Option<String>) {
    let next = if page["pageInfo"]["hasNextPage"] == true { page["pageInfo"]["endCursor"].as_str().map(str::to_string) } else { None };
    (issue_rows(acct, &page["nodes"]), next)
}

/// Every issue in a desk's scope — its project, or its whole team — in every
/// state, Done and Canceled included, and the ones Linear has since archived
/// (it archives closed issues after a while); trashed ones are not. Newest
/// update first, up to `cap`: the desk's reference index (decision 56).
pub async fn scope_issues(accounts: &[Account], b: &LinearBinding, cap: usize) -> Result<Vec<crate::refs::RefIssue>> {
    let acct = accounts
        .iter()
        .find(|a| a.label == b.account)
        .ok_or_else(|| anyhow!("no Linear account {:?} connected", b.account))?;
    let (field, id) = scope_filter(b);
    if id.is_empty() {
        return Ok(vec![]);
    }
    if acct.mcp.is_some() {
        return crate::refs::take_pages(cap, |after: Option<String>| linear_mcp::scope_page(acct, b, after)).await;
    }
    let q = format!(
        "query($p: ID, $a: String) {{ issues(first: 250, after: $a, orderBy: updatedAt, includeArchived: true, \
         filter: {{ {field}: {{ id: {{ eq: $p }} }} }}) {{ nodes {{ identifier title url trashed state {{ name }} }} \
         pageInfo {{ hasNextPage endCursor }} }} }}"
    );
    let q = q.as_str();
    crate::refs::take_pages(cap, |after: Option<String>| async move {
        let d = gql(acct, q, json!({ "p": id, "a": after })).await?;
        let page = &d["issues"];
        let rows = page["nodes"].as_array().into_iter().flatten().filter_map(ref_issue_from).collect();
        let next = if page["pageInfo"]["hasNextPage"] == true { page["pageInfo"]["endCursor"].as_str().map(str::to_string) } else { None };
        Ok((rows, next))
    })
    .await
}

fn ref_issue_from(n: &Value) -> Option<crate::refs::RefIssue> {
    if n["trashed"] == true {
        return None;
    }
    Some(crate::refs::RefIssue {
        key: n["identifier"].as_str()?.to_string(),
        title: n["title"].as_str().unwrap_or_default().to_string(),
        state: n["state"]["name"].as_str().unwrap_or_default().to_string(),
        url: n["url"].as_str().unwrap_or_default().to_string(),
    })
}

/// Every Linear project in every connected workspace, with its teams — what
/// the desk form picks from.
pub async fn projects(accounts: &[Account]) -> Result<Vec<Value>> {
    let q = "{ projects(first: 150, orderBy: updatedAt) { nodes { id name state teams { nodes { id key name } } } } }";
    let mut out = vec![];
    for acct in accounts {
        if acct.mcp.is_some() {
            out.extend(linear_mcp::projects(acct).await?);
            continue;
        }
        let d = gql(acct, q, json!({})).await?;
        for p in d["projects"]["nodes"].as_array().into_iter().flatten() {
            out.push(json!({
                "account": acct.label, "id": p["id"], "name": p["name"], "state": p["state"],
                "teams": p["teams"]["nodes"],
            }));
        }
    }
    Ok(out)
}

/// As many teams as one workspace is read for.
pub const TEAMS: usize = 250;

/// Every team in every connected workspace, a team with no project included —
/// what an agent checks a key against, and the form's whole-team choices
/// (decision 34).
pub async fn teams(accounts: &[Account]) -> Result<Vec<Value>> {
    let q = format!("{{ teams(first: {TEAMS}) {{ nodes {{ id key name }} }} }}");
    let mut out = vec![];
    for acct in accounts {
        let nodes = if acct.mcp.is_some() {
            linear_mcp::teams(acct).await?
        } else {
            gql(acct, &q, json!({})).await?["teams"]["nodes"].as_array().cloned().unwrap_or_default()
        };
        out.extend(team_rows(acct, &nodes));
    }
    Ok(out)
}

/// A workspace's teams in key order, each named with its account's label.
pub(crate) fn team_rows(acct: &Account, nodes: &[Value]) -> Vec<Value> {
    let mut rows: Vec<Value> = nodes
        .iter()
        .filter(|t| t["id"].is_string() && t["key"].is_string())
        .map(|t| json!({ "account": acct.label, "id": t["id"], "key": t["key"], "name": t["name"] }))
        .collect();
    rows.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    rows
}

/// The state a name means among a team's, spelled as the team spells it, or
/// the error listing the ones it has.
fn state_named(states: &[Value], team: &str, name: &str) -> Result<String> {
    let want = name.trim().to_lowercase();
    states
        .iter()
        .filter_map(|s| s["name"].as_str())
        .find(|n| n.to_lowercase() == want)
        .map(str::to_string)
        .ok_or_else(|| {
            let names: Vec<&str> = states.iter().filter_map(|s| s["name"].as_str()).collect();
            anyhow!("team {team} has no state {name:?}; it has: {}", names.join(", "))
        })
}

/// The query for one page of a team's issues in one state, narrowed to a
/// project by name when `project` is set.
fn state_query(project: bool) -> String {
    let p = if project { ", project: { name: { eqIgnoreCase: $p } }" } else { "" };
    let decl = if project { "($t: ID, $s: String, $p: String, $a: String)" } else { "($t: ID, $s: String, $a: String)" };
    format!(
        "query{decl} {{ issues(first: 50, after: $a, orderBy: updatedAt, \
         filter: {{ team: {{ id: {{ eq: $t }} }}, state: {{ name: {{ eq: $s }} }}{p} }}) {{ {ROW_FIELDS} \
         pageInfo {{ hasNextPage endCursor }} }} }}"
    )
}

/// Every issue of a team (by key) in a workflow state (by name, as the team
/// defines it, case ignored), optionally in one project (by name), newest
/// update first, up to `MINE_CAP`; and whether that cap cut the list. The
/// team is looked up in every connected workspace; the state must be one it
/// has, so a misspelling is an error and never an empty list.
pub async fn issues_in_state(accounts: &[Account], team_key: &str, state: &str, project: Option<&str>) -> Result<StateList> {
    let all = teams(accounts).await?;
    let key = team_key.trim();
    let Some(team) = all.iter().find(|t| t["key"].as_str().is_some_and(|k| k.eq_ignore_ascii_case(key))) else {
        let keys: Vec<&str> = all.iter().filter_map(|t| t["key"].as_str()).collect();
        bail!("no connected workspace has a team {key:?}; the teams are: {}", keys.join(", "));
    };
    let label = team["account"].as_str().unwrap_or_default();
    let team_id = team["id"].as_str().unwrap_or_default();
    let team_key = team["key"].as_str().unwrap_or_default().to_string();
    let acct = accounts.iter().find(|a| a.label == label).ok_or_else(|| anyhow!("no Linear account {label:?} connected"))?;
    let state = state_named(&team_states(acct, team_id).await?, &team_key, state)?;
    let project = project.map(str::trim).filter(|p| !p.is_empty());
    let (issues, truncated) = if acct.mcp.is_some() {
        crate::connectors::take_capped(MINE_CAP, |after: Option<String>| {
            linear_mcp::state_page(acct, team_id, &state, project, after)
        })
        .await?
    } else {
        let q = state_query(project.is_some());
        let q = q.as_str();
        let mut vars = json!({ "t": team_id, "s": state });
        if let Some(p) = project {
            vars["p"] = json!(p);
        }
        let vars = &vars;
        crate::connectors::take_capped(MINE_CAP, |after: Option<String>| async move {
            let mut v = vars.clone();
            v["a"] = json!(after);
            let d = gql(acct, q, v).await?;
            Ok(issue_page(acct, &d["issues"]))
        })
        .await?
    };
    Ok(StateList { workspace: label.to_string(), team: team_key, state, issues, truncated })
}

/// A team's issues in one state, as `issues_in_state` answers.
pub struct StateList {
    pub workspace: String,
    pub team: String,
    pub state: String,
    pub issues: Vec<Value>,
    pub truncated: bool,
}

/// Issues whose title contains the words — within the caller's desk's scope
/// when it has one — or the one a key names, wherever it is.
pub async fn search(accounts: &[Account], query: &str, scope: Option<&LinearBinding>) -> Result<Vec<Value>> {
    if let Some(key) = crate::store::issue_key_in(query).filter(|k| k.eq_ignore_ascii_case(query.trim())) {
        return Ok(match read_issue(accounts, &key, 0).await {
            Ok(v) => vec![v],
            Err(_) => vec![],
        });
    }
    let mut out = vec![];
    for acct in in_scope(accounts, scope) {
        if acct.mcp.is_some() {
            out.extend(linear_mcp::search(acct, query.trim(), scope).await?);
            continue;
        }
        let d = match scope {
            Some(b) => {
                let (field, id) = scope_filter(b);
                let q = format!(
                    "query($q: String!, $p: ID) {{ issues(first: 20, orderBy: updatedAt, \
                     filter: {{ title: {{ containsIgnoreCase: $q }}, {field}: {{ id: {{ eq: $p }} }} }}) {{ {ROW_FIELDS} }} }}"
                );
                gql(acct, &q, json!({ "q": query.trim(), "p": id })).await?
            }
            None => {
                let q = format!(
                    "query($q: String!) {{ issues(first: 20, orderBy: updatedAt, \
                     filter: {{ title: {{ containsIgnoreCase: $q }} }}) {{ {ROW_FIELDS} }} }}"
                );
                gql(acct, &q, json!({ "q": query.trim() })).await?
            }
        };
        out.extend(issue_rows(acct, &d["issues"]["nodes"]));
    }
    Ok(out)
}

/// A status move by the state's name, as an agent says it ("In Review").
pub async fn set_state_by_name(accounts: &[Account], key: &str, name: &str, now: i64) -> Result<IssueRef> {
    let issue = find_issue(accounts, key, now).await?.ok_or_else(|| anyhow::Error::from(NoWorkspaceHas(key.to_string())))?;
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

/// Post an approved comment, under `parent` when it replies in a thread,
/// answering its id and link. Only the Desk tab's Send reaches this.
pub async fn post_comment(accounts: &[Account], key: &str, body: &str, parent: Option<&str>) -> Result<(String, String)> {
    first_with(accounts, key, |acct| async move {
        if acct.mcp.is_some() {
            linear_mcp::comment(acct, key, body, parent).await
        } else {
            gql_comment(acct, key, body, parent).await
        }
    })
    .await
}

/// The issue's id in this workspace; `None` when it has no such key.
async fn gql_issue_id(acct: &Account, key: &str) -> Result<Option<Value>> {
    match gql(acct, "query($id: String!) { issue(id: $id) { id } }", json!({ "id": key })).await {
        Ok(d) if d["issue"].is_object() => Ok(Some(d["issue"]["id"].clone())),
        Ok(_) => Ok(None),
        Err(e) if not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `None` when this workspace has no such key.
async fn gql_comment(acct: &Account, key: &str, body: &str, parent: Option<&str>) -> Result<Option<(String, String)>> {
    let Some(id) = gql_issue_id(acct, key).await? else { return Ok(None) };
    // Only the fields this comment sets, as an edit sends.
    let mut input = json!({ "issueId": id, "body": body });
    if let Some(p) = parent {
        input["parentId"] = json!(p);
    }
    let d = gql(
        acct,
        "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success comment { id url } } }",
        json!({ "input": input }),
    )
    .await?;
    if d["commentCreate"]["success"].as_bool() != Some(true) {
        bail!("Linear refused the comment");
    }
    let c = &d["commentCreate"]["comment"];
    Ok(Some((c["id"].as_str().unwrap_or_default().to_string(), c["url"].as_str().unwrap_or_default().to_string())))
}

/// Change an approved issue's title, description or both, answering its
/// link. `None` leaves that field as it is. Only the Desk tab's Send, or a
/// desk set to write directly, reaches this (decision 75).
pub async fn edit_issue(accounts: &[Account], key: &str, title: Option<&str>, description: Option<&str>) -> Result<String> {
    first_with(accounts, key, |acct| async move {
        if acct.mcp.is_some() {
            linear_mcp::edit_issue(acct, key, title, description).await
        } else {
            gql_edit(acct, key, title, description).await
        }
    })
    .await
}

/// `None` when this workspace has no such key.
async fn gql_edit(acct: &Account, key: &str, title: Option<&str>, description: Option<&str>) -> Result<Option<String>> {
    let Some(id) = gql_issue_id(acct, key).await? else { return Ok(None) };
    let d = gql(
        acct,
        "mutation($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success issue { url } } }",
        json!({ "id": id, "input": edit_input(title, description) }),
    )
    .await?;
    if d["issueUpdate"]["success"].as_bool() != Some(true) {
        bail!("Linear refused the edit to {key}");
    }
    Ok(Some(d["issueUpdate"]["issue"]["url"].as_str().unwrap_or_default().to_string()))
}

/// The fields an edit sets; one it leaves alone is absent, never null.
pub(crate) fn edit_input(title: Option<&str>, description: Option<&str>) -> Value {
    let mut input = json!({});
    if let Some(t) = title {
        input["title"] = json!(t);
    }
    if let Some(d) = description {
        input["description"] = json!(d);
    }
    input
}

/// Create an approved issue in the team with this key, in whichever
/// workspace has that team.
pub async fn create_issue(
    accounts: &[Account],
    team_key: &str,
    title: &str,
    description: &str,
    project_id: Option<&str>,
) -> Result<(String, String)> {
    for acct in accounts {
        if acct.mcp.is_some() {
            let Some(team_id) = linear_mcp::team_id(acct, team_key).await? else { continue };
            return linear_mcp::create_issue(acct, &team_id, title, description, project_id).await;
        }
        let d = gql(
            acct,
            "query($k: String!) { teams(filter: { key: { eqIgnoreCase: $k } }) { nodes { id key } } }",
            json!({ "k": team_key }),
        )
        .await?;
        let Some(team) = d["teams"]["nodes"].as_array().and_then(|n| n.first().cloned()) else { continue };
        let d = gql(
            acct,
            "mutation($t: String!, $title: String!, $d: String, $p: String) { issueCreate(input: { teamId: $t, title: $title, description: $d, projectId: $p }) \
             { success issue { identifier url } } }",
            json!({ "t": team["id"], "title": title, "d": description, "p": project_id }),
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

/// The state Start work moves an issue to: the team's "In Progress", else
/// its first started state — and only forward: an issue already started,
/// done or canceled stays where it is.
pub fn start_state(states: &[Value], current_type: &str) -> Option<String> {
    if !matches!(current_type, "triage" | "backlog" | "unstarted" | "") {
        return None;
    }
    let started: Vec<&Value> = states.iter().filter(|s| s["type"] == "started").collect();
    started
        .iter()
        .find(|s| s["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("in progress")))
        .or_else(|| started.first())
        .and_then(|s| s["id"].as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_names_its_project_and_the_open_query_is_not_limited_to_the_viewer() {
        let acct = Account::new("personal", "t");
        let nodes = json!([
            { "identifier": "EMB-1", "title": "T", "url": "u", "updatedAt": "d", "state": { "name": "Todo" }, "project": { "name": "embarch-ui" } },
            { "identifier": "EMB-2", "title": "T", "url": "u", "updatedAt": "d", "state": { "name": "Todo" }, "project": null },
        ]);
        let rows = issue_rows(&acct, &nodes);
        assert_eq!(rows[0]["project"], "embarch-ui");
        assert!(rows[1]["project"].is_null());
        let all = open_query("($a: String)", OPEN_STATES, false);
        assert!(all.contains("{ issues(") && !all.contains("viewer"), "{all}");
        let mine = open_query("($a: String)", OPEN_STATES, true);
        assert!(mine.contains("viewer { assignedIssues("), "{mine}");
    }

    #[test]
    fn every_team_is_a_row_in_key_order_whether_or_not_it_holds_a_project() {
        let acct = Account::new("personal", "t");
        let nodes = json!([
            { "id": "t2", "key": "ZED", "name": "Zed client" },
            { "id": "t1", "key": "ABC", "name": "Abc client" },
            { "id": null, "key": "BAD", "name": "No id" },
        ]);
        let rows = team_rows(&acct, nodes.as_array().unwrap());
        assert_eq!(rows, vec![
            json!({ "account": "personal", "id": "t1", "key": "ABC", "name": "Abc client" }),
            json!({ "account": "personal", "id": "t2", "key": "ZED", "name": "Zed client" }),
        ]);
    }

    #[tokio::test]
    async fn assigned_issues_page_on_the_cursor_until_the_cap() {
        let acct = Account::new("w", "t");
        let page = |after: Option<String>| {
            let n: u32 = after.as_deref().map(|a| a.parse().unwrap()).unwrap_or(0);
            let nodes: Vec<Value> = (0..50).map(|i| json!({ "identifier": format!("X-{}", n * 50 + i) })).collect();
            let more = n < 1;
            json!({ "nodes": nodes, "pageInfo": { "hasNextPage": more, "endCursor": format!("{}", n + 1) } })
        };
        let (rows, cut) = crate::connectors::take_capped(MINE_CAP, |a| {
            let p = page(a);
            let acct = &acct;
            async move { Ok(issue_page(acct, &p)) }
        })
        .await
        .unwrap();
        assert_eq!(rows.len(), 100);
        assert!(!cut);
        assert_eq!(rows[99]["key"], "X-99");
    }

    #[test]
    fn a_state_is_matched_by_name_as_the_team_spells_it_and_a_miss_lists_the_team_states() {
        let states = vec![json!({"name": "Backlog"}), json!({"name": "Fleet Ready"})];
        assert_eq!(state_named(&states, "EMB", " fleet ready ").unwrap(), "Fleet Ready");
        let e = state_named(&states, "EMB", "Ready").unwrap_err().to_string();
        assert!(e.contains("EMB") && e.contains("Backlog, Fleet Ready"), "{e}");
    }

    #[test]
    fn the_state_query_filters_on_team_and_state_and_on_project_only_when_asked() {
        let q = state_query(false);
        assert!(q.contains("team: { id: { eq: $t } }") && q.contains("state: { name: { eq: $s } }") && !q.contains("eqIgnoreCase"), "{q}");
        let q = state_query(true);
        assert!(q.contains("project: { name: { eqIgnoreCase: $p } }") && q.contains("$p: String"), "{q}");
    }

    #[tokio::test]
    async fn a_state_list_stops_at_the_cap_and_says_so() {
        let acct = Account::new("w", "t");
        // An endless queue: every page is full and has a next.
        let page = |after: Option<String>| {
            let n: u32 = after.as_deref().map(|a| a.parse().unwrap()).unwrap_or(0);
            let nodes: Vec<Value> = (0..50).map(|i| json!({ "identifier": format!("X-{}", n * 50 + i) })).collect();
            json!({ "nodes": nodes, "pageInfo": { "hasNextPage": true, "endCursor": format!("{}", n + 1) } })
        };
        let (rows, cut) = crate::connectors::take_capped(MINE_CAP, |a| {
            let p = page(a);
            let acct = &acct;
            async move { Ok(issue_page(acct, &p)) }
        })
        .await
        .unwrap();
        assert_eq!(rows.len(), MINE_CAP);
        assert!(cut);
    }

    #[test]
    fn start_work_moves_an_issue_forward_to_in_progress_only() {
        let states = vec![
            json!({"id": "a", "name": "Todo", "type": "unstarted"}),
            json!({"id": "b", "name": "Investigating", "type": "started"}),
            json!({"id": "c", "name": "In Progress", "type": "started"}),
            json!({"id": "d", "name": "Done", "type": "completed"}),
        ];
        assert_eq!(start_state(&states, "unstarted").as_deref(), Some("c"));
        assert_eq!(start_state(&states, "backlog").as_deref(), Some("c"));
        assert_eq!(start_state(&states, "started"), None);
        assert_eq!(start_state(&states, "completed"), None);
        assert_eq!(start_state(&states[..2], "unstarted").as_deref(), Some("b"));
    }

    #[test]
    fn an_index_row_reads_out_of_the_graphql_shape_and_a_trashed_issue_is_none() {
        let n = json!({"identifier": "ABC-12", "title": "Add the widget", "url": "https://linear.app/ws/issue/ABC-12",
                       "trashed": null, "state": {"name": "Done"}});
        let r = ref_issue_from(&n).unwrap();
        assert_eq!((r.key.as_str(), r.title.as_str(), r.state.as_str()), ("ABC-12", "Add the widget", "Done"));
        let mut t = n.clone();
        t["trashed"] = json!(true);
        assert!(ref_issue_from(&t).is_none());
    }

    #[test]
    fn an_issue_reads_out_of_the_graphql_shape() {
        let acct = Account::new("work", "t");
        let v = json!({
            "id": "uuid", "identifier": "ABC-123", "title": "Bootload", "url": "https://linear.app/x/issue/ABC-123",
            "state": {"id": "s1", "name": "In Progress", "type": "started"},
            "team": {"id": "t1", "key": "CHA"}
        });
        let i = issue_from(&acct, &v, 5).unwrap();
        assert_eq!((i.key.as_str(), i.state_name.as_str(), i.account.as_str()), ("ABC-123", "In Progress", "work"));
        assert!(issue_from(&acct, &Value::Null, 5).is_none());
    }

    #[test]
    fn an_edit_sets_only_the_fields_it_names_and_a_comment_carries_its_thread() {
        assert_eq!(edit_input(Some("T"), None), json!({"title": "T"}), "a field left alone is absent, never null");
        assert_eq!(edit_input(None, Some("D")), json!({"description": "D"}));
        assert_eq!(edit_input(Some("T"), Some("D")), json!({"title": "T", "description": "D"}));
        let i = json!({
            "identifier": "ABC-1", "description": "d",
            "comments": {"nodes": [
                {"id": "c2", "body": "reply", "createdAt": "2", "user": {"name": "G"}, "parent": {"id": "c1"}},
                {"id": "c1", "body": "head", "createdAt": "1", "user": {"name": "G"}, "parent": null}
            ]}
        });
        let rows: Vec<Value> = i["comments"]["nodes"].as_array().unwrap().clone();
        let v = detail_from(&Account::new("work", "t"), &i, &rows, false, 0);
        assert_eq!((v["comments"][0]["id"].as_str(), v["comments"][0]["parent"].as_str()), (Some("c1"), None));
        assert_eq!((v["comments"][1]["id"].as_str(), v["comments"][1]["parent"].as_str()), (Some("c2"), Some("c1")));
    }

    fn numbered(n: usize) -> Vec<Value> {
        // Newest first, as the server lists them.
        (0..n).rev().map(|i| json!({ "body": format!("c{i}") })).collect()
    }

    #[test]
    fn an_uncut_comment_list_says_nothing_is_missing() {
        let v = comment_fields(&numbered(3), false, 0);
        assert_eq!((v["comments_total"].as_u64(), v["comments_truncated"].as_bool()), (Some(3), Some(false)));
        assert!(v.get("comments_note").is_none());
        assert_eq!((v["comments"][0]["body"].as_str(), v["comments"][2]["body"].as_str()), (Some("c0"), Some("c2")));
    }

    #[test]
    fn a_cut_comment_list_gives_the_total_and_the_skip_that_reads_the_rest() {
        let all = numbered(60);
        let v = comment_fields(&all, false, 0);
        assert_eq!((v["comments_total"].as_u64(), v["comments_truncated"].as_bool()), (Some(60), Some(true)));
        assert_eq!(v["comments"].as_array().unwrap().len(), 25);
        assert_eq!((v["comments"][0]["body"].as_str(), v["comments"][24]["body"].as_str()), (Some("c35"), Some("c59")));
        assert!(v["comments_note"].as_str().unwrap().contains("`skip` set to 25"));
        let v = comment_fields(&all, false, 25);
        assert_eq!((v["comments"][0]["body"].as_str(), v["comments"][24]["body"].as_str()), (Some("c10"), Some("c34")));
        let v = comment_fields(&all, false, 50);
        assert_eq!(v["comments"].as_array().unwrap().len(), 10);
        assert_eq!(v["comments"][0]["body"], "c0");
        assert_eq!(v["comments_truncated"], true, "newer ones are still not returned");
        // Exactly one window and nothing else is not a cut.
        assert_eq!(comment_fields(&numbered(25), false, 0)["comments_truncated"], false);
    }

    #[test]
    fn a_comment_count_stopped_by_the_cap_is_a_lower_bound() {
        let v = comment_fields(&numbered(COMMENTS_CAP), true, 0);
        assert_eq!((v["comments_capped"].as_bool(), v["comments_truncated"].as_bool()), (Some(true), Some(true)));
        assert!(v["comments_note"].as_str().unwrap().contains("500+"));
    }

    #[test]
    fn a_key_no_workspace_has_says_which_case_it_is() {
        use crate::store::{Desk, LinearBinding};
        let desk = |id: &str, name: &str, team: &str, repos: &[&str]| Desk {
            id: id.into(),
            name: name.into(),
            linear: Some(LinearBinding { team_key: team.into(), ..Default::default() }),
            firmware_repos: repos.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        };
        let (a, b, c) = (desk("a", "Alpha", "AAA", &["/w/a"]), desk("b", "Beta", "BBB", &["/w/b", "/w/b2"]), desk("c", "Gamma", "CCC", &[]));
        let desks = [&a, &b, &c];
        let teams = [json!({ "key": "AAA" }), json!({ "key": "ZZZ" })];
        // Another desk's team: the desk and its first folder, never the key's own desk.
        let m = no_key_message("bbb-7", Some(&teams), &desks, Some("a"));
        assert!(m.contains("\"Beta\"") && m.contains("/w/b") && !m.contains("b2"), "{m}");
        let m = no_key_message("CCC-1", None, &desks, Some("a"));
        assert!(m.contains("\"Gamma\"") && m.contains("no folder"), "{m}");
        // No connected workspace has the team: says so, names no desk.
        let m = no_key_message("QQQ-3", Some(&teams), &desks, Some("a"));
        assert!(m.contains("has a team QQQ") && !m.contains("Alpha") && !m.contains("desk"), "{m}");
        // The caller's own team, or a team that is here: the issue is what is missing.
        assert_eq!(no_key_message("AAA-9", Some(&teams), &desks, Some("a")), "no connected Linear workspace has AAA-9");
        assert_eq!(no_key_message("ZZZ-9", Some(&teams), &desks, None), "no connected Linear workspace has ZZZ-9");
        // The typed error reads as it always did.
        assert_eq!(NoWorkspaceHas("X-1".into()).to_string(), "no connected Linear workspace has X-1");
    }
}
