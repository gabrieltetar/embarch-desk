//! Linear through its hosted MCP server, for a workspace that gives the user
//! no personal API key — a guest in a client's workspace (decision 68).
//!
//! The sign-in is OAuth against `mcp.linear.app` itself, which registers the
//! desk as a client on the spot (dynamic client registration), so EmbArch
//! owns no app. Its token reaches only the MCP endpoint — Linear's GraphQL API
//! answers 401 to it [measured 2026-10-31] — and lasts 86,100 s, so each of
//! `linear`'s operations is one or two of the server's tools here, answered in
//! the shapes the GraphQL side gives, and the token renews itself.

use crate::connectors::{http, Account, Connectors, McpGrant};
use crate::linear::scope_filter;
use crate::refs::RefIssue;
use crate::store::{IssueRef, LinearBinding};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

const SERVER: &str = "https://mcp.linear.app";

/// A token is renewed this long before it runs out.
const RENEW_EARLY_MS: i64 = 5 * 60_000;

/// The token's life when the server does not say: an hour, well under the
/// day it gives [measured 2026-10-31].
const DEFAULT_LIFE_S: i64 = 3_600;

// ---------------------------------------------------------------- sign-in

/// Register a client for one sign-in, sending the browser back to `redirect`.
pub async fn register(redirect: &str) -> Result<String> {
    let resp = http()
        .post(format!("{SERVER}/register"))
        .json(&json!({
            "client_name": "EmbArch Desk",
            "redirect_uris": [redirect],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Linear's MCP server refused the registration: HTTP {status} {body}");
    }
    body["client_id"].as_str().map(str::to_string).ok_or_else(|| anyhow!("Linear's MCP server registered no client_id"))
}

/// A PKCE verifier and its S256 challenge.
pub fn pkce() -> Result<(String, String)> {
    let verifier = crate::paths::random_hex(32)?;
    let challenge = challenge(&verifier);
    Ok((verifier, challenge))
}

fn challenge(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// Where the browser signs in and picks the workspace.
pub fn authorize_url(client_id: &str, redirect: &str, challenge: &str, state: &str) -> String {
    let mut u = reqwest::Url::parse(&format!("{SERVER}/authorize")).expect("a fixed url parses");
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", "read write")
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", &format!("{SERVER}/mcp"));
    u.into()
}

/// The code the browser brought back, for a token and the grant that renews it.
pub async fn exchange(client_id: &str, redirect: &str, code: &str, verifier: &str, now: i64) -> Result<(String, McpGrant)> {
    let resource = format!("{SERVER}/mcp");
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", client_id),
        ("code_verifier", verifier),
        ("resource", resource.as_str()),
    ];
    grant(client_id, &form, None, now).await
}

async fn renew(g: &McpGrant, now: i64) -> Result<(String, McpGrant)> {
    let form = [("grant_type", "refresh_token"), ("refresh_token", g.refresh_token.as_str()), ("client_id", g.client_id.as_str())];
    grant(&g.client_id, &form, Some(&g.refresh_token), now).await
}

/// One call to the token endpoint. A renewal that hands back no new refresh
/// token keeps the old one.
async fn grant(client_id: &str, form: &[(&str, &str)], refresh: Option<&str>, now: i64) -> Result<(String, McpGrant)> {
    let resp = http()
        .post(format!("{SERVER}/token"))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form_body(form))
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        let why = body["error_description"].as_str().or(body["error"].as_str()).unwrap_or("no reason given");
        bail!("Linear's MCP server refused the token: HTTP {status}, {why}");
    }
    let access = body["access_token"].as_str().ok_or_else(|| anyhow!("Linear's MCP server gave no access token"))?;
    let refresh_token = body["refresh_token"]
        .as_str()
        .or(refresh)
        .ok_or_else(|| anyhow!("Linear's MCP server gave no refresh token"))?;
    let life_s = body["expires_in"].as_i64().unwrap_or(DEFAULT_LIFE_S);
    Ok((
        access.to_string(),
        McpGrant { client_id: client_id.into(), refresh_token: refresh_token.into(), expires_ms: now + life_s * 1000 },
    ))
}

/// A form body, encoded by the URL parser reqwest already carries: its own
/// `form` feature would change the suite's shared reqwest feature set.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut u = reqwest::Url::parse("form:").expect("a fixed url parses");
    u.query_pairs_mut().extend_pairs(pairs);
    u.query().unwrap_or_default().to_string()
}

// ---------------------------------------------------------------- the token

/// One renewal at a time: a second call that finds the token stale waits
/// here, then finds the first one's token in the file.
static RENEWING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A token good for the next few minutes. `rejected` says the server just
/// refused `acct.token`, so it is renewed even if it should still be good.
async fn token(acct: &Account, rejected: bool) -> Result<String> {
    let now = crate::server::now_ms();
    let fresh = |a: &Account| a.mcp.as_ref().is_some_and(|g| g.expires_ms - now > RENEW_EARLY_MS);
    if !rejected && fresh(acct) {
        return Ok(acct.token.clone());
    }
    let _one = RENEWING.lock().await;
    // The caller's copy of the account may predate a renewal another call made.
    let mut conns = Connectors::load()?;
    let Some(cur) = conns.linear.iter().find(|a| a.label == acct.label).cloned() else {
        bail!("the Linear account {:?} is no longer connected", acct.label);
    };
    if fresh(&cur) && (!rejected || cur.token != acct.token) {
        return Ok(cur.token);
    }
    let g = cur.mcp.as_ref().ok_or_else(|| anyhow!("{:?} is not a Linear MCP sign-in", acct.label))?;
    let (access, grant) = renew(g, now)
        .await
        .with_context(|| format!("{:?}'s Linear sign-in could not be renewed; sign in again under the Desk tab's Connectors", acct.label))?;
    conns.put("linear", Account { label: cur.label.clone(), token: access.clone(), mcp: Some(grant) })?;
    conns.save()?;
    Ok(access)
}

// ---------------------------------------------------------------- the call

/// What a tool itself answered with `isError`, as opposed to a failed call.
#[derive(Debug)]
struct ToolError(String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

/// Whether a tool said the issue is not in this workspace — the server's
/// "Could not find referenced Issue." — which is how a workspace says "not mine".
fn not_here(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ToolError>().is_some_and(|t| {
        let m = t.0.to_lowercase();
        m.contains("could not find") || m.contains("not found")
    })
}

/// One tool call, its answer parsed from the JSON text the server returns.
/// A token refused before its time is renewed once and the call made again.
async fn call(acct: &Account, tool: &str, args: Value) -> Result<Value> {
    let mut tok = token(acct, false).await?;
    let mut renewed = false;
    loop {
        let resp = http()
            .post(format!("{SERVER}/mcp"))
            .bearer_auth(&tok)
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": &args } }))
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED && !renewed {
            tok = token(acct, true).await?;
            renewed = true;
            continue;
        }
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("linear ({}): {tool}: HTTP {status}", acct.label);
        }
        return tool_result(&acct.label, tool, &body);
    }
}

/// The JSON-RPC answer in a body that is plain JSON or a server-sent event stream.
fn rpc_answer(body: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return Some(v);
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .find(|v| v.get("result").is_some() || v.get("error").is_some())
}

fn tool_result(label: &str, tool: &str, body: &str) -> Result<Value> {
    let v = rpc_answer(body).ok_or_else(|| anyhow!("linear ({label}): {tool}: no answer in the MCP reply"))?;
    if let Some(e) = v.get("error") {
        bail!("linear ({label}): {tool}: {}", e["message"].as_str().unwrap_or("error"));
    }
    let r = &v["result"];
    let text = r["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
    if r["isError"] == true {
        return Err(anyhow::Error::new(ToolError(text)).context(format!("linear ({label}): {tool}")));
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// A tool's answer, `None` when the issue it names is not in this workspace.
async fn call_on_issue(acct: &Account, tool: &str, args: Value) -> Result<Option<Value>> {
    match call(acct, tool, args).await {
        Ok(v) => Ok(Some(v)),
        Err(e) if not_here(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------- the operations

/// The workspace's name, which is what a connector check reports.
pub async fn whoami(acct: &Account) -> Result<String> {
    let v = call(acct, "whoami", json!({})).await?;
    Ok(format!(
        "{} @ {}, through Linear's MCP",
        v["user"]["name"].as_str().unwrap_or("?"),
        v["workspace"]["name"].as_str().unwrap_or("?")
    ))
}

const ISSUE_FIELDS: [&str; 9] = ["uuid", "title", "url", "gitBranchName", "status", "statusType", "teamId", "projectId", "project"];

/// An issue by key or id; `None` when this workspace has no such issue.
pub async fn issue_ref(acct: &Account, id: &str, now: i64) -> Result<Option<IssueRef>> {
    let Some(v) = call_on_issue(acct, "get_issue", json!({ "id": id, "fields": ISSUE_FIELDS })).await? else {
        return Ok(None);
    };
    let Some(mut i) = issue_from(acct, &v, now) else { return Ok(None) };
    // The server names the state; the card's dropdown selects by its id.
    if !i.team_id.is_empty() {
        let states = team_states(acct, &i.team_id).await?;
        i.state_id = state_id_named(&states, &i.state_name);
    }
    Ok(Some(i))
}

fn issue_from(acct: &Account, v: &Value, now: i64) -> Option<IssueRef> {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Some(IssueRef {
        account: acct.label.clone(),
        // The server's `id` is the key; the UUID is `uuid`.
        key: v["id"].as_str()?.to_string(),
        id: v["uuid"].as_str()?.to_string(),
        url: v["url"].as_str()?.to_string(),
        title: v["title"].as_str()?.to_string(),
        state_id: String::new(),
        state_name: s("status"),
        state_type: s("statusType"),
        team_id: s("teamId"),
        project_id: s("projectId"),
        project_name: s("project"),
        branch_name: s("gitBranchName"),
        refreshed_ms: now,
    })
}

fn state_id_named(states: &[Value], name: &str) -> String {
    states.iter().find(|s| s["name"] == name).and_then(|s| s["id"].as_str()).unwrap_or_default().to_string()
}

/// Linear's board columns run by state type; the server gives no position.
const BOARD_ORDER: [&str; 7] = ["triage", "backlog", "unstarted", "started", "completed", "canceled", "duplicate"];

/// The team's workflow states, in board order.
pub async fn team_states(acct: &Account, team_id: &str) -> Result<Vec<Value>> {
    Ok(in_board_order(call(acct, "list_issue_statuses", json!({ "team": team_id })).await?))
}

fn in_board_order(v: Value) -> Vec<Value> {
    let mut states = match v {
        Value::Array(a) => a,
        _ => vec![],
    };
    states.sort_by_key(|s| BOARD_ORDER.iter().position(|t| s["type"] == *t).unwrap_or(BOARD_ORDER.len()));
    states
}

pub async fn move_state(acct: &Account, issue: &IssueRef, state_id: &str, now: i64) -> Result<IssueRef> {
    call(acct, "save_issue", json!({ "id": issue.id, "state": state_id })).await?;
    issue_ref(acct, &issue.id, now).await?.ok_or_else(|| anyhow!("Linear returned no issue"))
}

/// As many comments as the GraphQL side reads.
const COMMENTS: usize = 25;

/// An issue as an agent reads it, in `linear::read_issue`'s shape; `None`
/// when this workspace has no such key.
pub async fn issue_detail(acct: &Account, key: &str) -> Result<Option<Value>> {
    let fields = ["title", "url", "description", "priority", "status", "project", "assignee", "labels", "updatedAt"];
    let Some(i) = call_on_issue(acct, "get_issue", json!({ "id": key, "fields": fields })).await? else {
        return Ok(None);
    };
    let c = call(acct, "list_comments", json!({ "issueId": key, "limit": COMMENTS, "orderBy": "createdAt" })).await?;
    Ok(Some(detail_from(acct, &i, &c)))
}

fn detail_from(acct: &Account, i: &Value, c: &Value) -> Value {
    // The server lists comments newest first; an agent reads them oldest first.
    let comments: Vec<Value> = c["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| json!({ "id": c["id"], "parent": parent_of(c), "author": c["author"]["name"], "at": c["createdAt"], "body": c["body"] }))
        .rev()
        .collect();
    let key = i["id"].as_str().unwrap_or_default();
    json!({
        "workspace": acct.label,
        "key": key, "title": i["title"], "url": i["url"],
        "state": i["status"], "priority": i["priority"]["name"],
        // The server gives the team's name; a key starts with the team's key.
        "team": key.rsplit_once('-').map(|(team, _)| team), "project": i["project"], "assignee": i["assignee"],
        "labels": i["labels"],
        "description": i["description"],
        "comments": comments,
        "updated": i["updatedAt"],
    })
}

/// A page as big as the GraphQL side's.
const PAGE: usize = 50;
const ROW_FIELDS: [&str; 6] = ["title", "url", "status", "statusType", "updatedAt", "project"];
/// What GraphQL's open-issue filter leaves out; the server has no "not in" filter.
const CLOSED: [&str; 3] = ["completed", "canceled", "duplicate"];

fn row(acct: &Account, i: &Value) -> Value {
    json!({
        "workspace": acct.label, "key": i["id"], "title": i["title"],
        "state": i["status"], "url": i["url"], "updated": i["updatedAt"], "project": i["project"],
    })
}

fn next_cursor(v: &Value) -> Option<String> {
    if v["hasNextPage"] == true {
        v["cursor"].as_str().map(str::to_string)
    } else {
        None
    }
}

fn scoped(mut args: Value, scope: Option<&LinearBinding>, after: Option<String>) -> Value {
    if let Some(b) = scope {
        let (field, id) = scope_filter(b);
        args[field] = json!(id);
    }
    if let Some(a) = after {
        args["cursor"] = json!(a);
    }
    args
}

/// One page of open issues, the user's or everyone's, and the next page's cursor.
pub async fn open_page(acct: &Account, scope: Option<&LinearBinding>, mine: bool, after: Option<String>) -> Result<(Vec<Value>, Option<String>)> {
    let mut args = json!({ "limit": PAGE, "orderBy": "updatedAt", "fields": ROW_FIELDS });
    if mine {
        args["assignee"] = json!("me");
    }
    let v = call(acct, "list_issues", scoped(args, scope, after)).await?;
    Ok((open_rows(acct, &v), next_cursor(&v)))
}

fn open_rows(acct: &Account, v: &Value) -> Vec<Value> {
    v["issues"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| !CLOSED.contains(&i["statusType"].as_str().unwrap_or_default()))
        .map(|i| row(acct, i))
        .collect()
}

/// The reference index's page size, as on the GraphQL side.
const SCOPE_PAGE: usize = 250;

/// One page of every issue in a desk's scope, archived ones included.
pub async fn scope_page(acct: &Account, b: &LinearBinding, after: Option<String>) -> Result<(Vec<RefIssue>, Option<String>)> {
    let args = json!({ "limit": SCOPE_PAGE, "orderBy": "updatedAt", "includeArchived": true, "fields": ["title", "url", "status"] });
    let v = call(acct, "list_issues", scoped(args, Some(b), after)).await?;
    let rows = v["issues"].as_array().into_iter().flatten().filter_map(ref_issue_from).collect();
    Ok((rows, next_cursor(&v)))
}

fn ref_issue_from(i: &Value) -> Option<RefIssue> {
    Some(RefIssue {
        key: i["id"].as_str()?.to_string(),
        title: i["title"].as_str().unwrap_or_default().to_string(),
        state: i["status"].as_str().unwrap_or_default().to_string(),
        url: i["url"].as_str().unwrap_or_default().to_string(),
    })
}

/// As many projects as the GraphQL side reads.
const PROJECTS: usize = 150;

pub async fn projects(acct: &Account) -> Result<Vec<Value>> {
    crate::refs::take_pages(PROJECTS, |after: Option<String>| async move {
        let args = json!({ "limit": PAGE, "orderBy": "updatedAt", "fields": ["uuid", "name", "status", "teams"] });
        let v = call(acct, "list_projects", scoped(args, None, after)).await?;
        Ok((project_rows(acct, &v), next_cursor(&v)))
    })
    .await
}

fn project_rows(acct: &Account, v: &Value) -> Vec<Value> {
    v["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            // `id` is the project's P- identifier; an issue names its project by UUID.
            json!({ "account": acct.label, "id": p["uuid"], "name": p["name"], "state": p["status"]["type"], "teams": p["teams"] })
        })
        .collect()
}

/// Every team in the workspace, as the server lists them; a team needs no
/// project to be here.
pub async fn teams(acct: &Account) -> Result<Vec<Value>> {
    crate::refs::take_pages(crate::linear::TEAMS, |after: Option<String>| async move {
        let v = call(acct, "list_teams", scoped(json!({ "limit": PAGE }), None, after)).await?;
        Ok((v["teams"].as_array().cloned().unwrap_or_default(), next_cursor(&v)))
    })
    .await
}

/// As many matches as the GraphQL side returns.
const SEARCH: usize = 20;

/// Issues matching the words. Unlike the GraphQL side's title match, the
/// server's `query` searches the description too.
pub async fn search(acct: &Account, query: &str, scope: Option<&LinearBinding>) -> Result<Vec<Value>> {
    let args = json!({ "query": query, "limit": SEARCH, "orderBy": "updatedAt", "fields": ROW_FIELDS });
    let v = call(acct, "list_issues", scoped(args, scope, None)).await?;
    Ok(v["issues"].as_array().into_iter().flatten().map(|i| row(acct, i)).collect())
}

/// The comment a listed comment replies under, as the server spells it.
fn parent_of(c: &Value) -> Value {
    [&c["parentId"], &c["parent"]["id"], &c["parent"]].into_iter().find(|v| v.is_string()).cloned().unwrap_or(Value::Null)
}

/// Post a comment, under `parent` when it replies in a thread, answering its
/// id and link; `None` when this workspace has no such issue.
pub async fn comment(acct: &Account, key: &str, body: &str, parent: Option<&str>) -> Result<Option<(String, String)>> {
    // A reply names only its parent: the server ignores the issue then.
    let args = match parent {
        Some(p) => json!({ "parentId": p, "body": body }),
        None => json!({ "issueId": key, "body": body }),
    };
    let Some(v) = call_on_issue(acct, "save_comment", args).await? else {
        return Ok(None);
    };
    let field = |f: &str| v[f].as_str().or(v["comment"][f].as_str()).unwrap_or_default().to_string();
    Ok(Some((field("id"), field("url"))))
}

/// Change an issue's title, description or both, answering its link; `None`
/// when this workspace has no such issue.
pub async fn edit_issue(acct: &Account, key: &str, title: Option<&str>, description: Option<&str>) -> Result<Option<String>> {
    let mut args = crate::linear::edit_input(title, description);
    args["id"] = json!(key);
    let Some(v) = call_on_issue(acct, "save_issue", args).await? else {
        return Ok(None);
    };
    Ok(Some(v["url"].as_str().or(v["issue"]["url"].as_str()).unwrap_or_default().to_string()))
}

/// The id of this workspace's team with that key, if it has one.
pub async fn team_id(acct: &Account, key: &str) -> Result<Option<String>> {
    let v = call(acct, "list_teams", json!({ "query": key })).await?;
    Ok(v["teams"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["key"].as_str().is_some_and(|k| k.eq_ignore_ascii_case(key)))
        .and_then(|t| t["id"].as_str())
        .map(str::to_string))
}

/// Create an issue, answering its key and link.
pub async fn create_issue(acct: &Account, team_id: &str, title: &str, description: &str, project_id: Option<&str>) -> Result<(String, String)> {
    let mut args = json!({ "team": team_id, "title": title, "description": description });
    if let Some(p) = project_id {
        args["project"] = json!(p);
    }
    let v = call(acct, "save_issue", args).await?;
    let key = v["id"].as_str().filter(|k| crate::store::issue_key_in(k).is_some()).or(v["identifier"].as_str());
    Ok((key.unwrap_or_default().to_string(), v["url"].as_str().unwrap_or_default().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> Account {
        Account::new("client", "t")
    }

    #[test]
    fn an_answer_reads_out_of_an_event_stream_or_plain_json() {
        let text = r#"{"user":{"name":"me"},"workspace":{"name":"Client"}}"#;
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": text }] } });
        let sse = format!("event: message\ndata: {msg}\n\n");
        for body in [sse.as_str(), &msg.to_string()] {
            let v = tool_result("client", "whoami", body).unwrap();
            assert_eq!(v["workspace"]["name"], "Client");
        }
        assert!(tool_result("client", "whoami", "event: ping\n\n").is_err());
        let rpc_err = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "bad params" } });
        assert!(tool_result("client", "x", &rpc_err.to_string()).unwrap_err().to_string().contains("bad params"));
    }

    /// The server's answer for a key from another workspace is "not mine";
    /// any other failure, a renewal's included, is not.
    #[test]
    fn only_a_tool_saying_could_not_find_is_not_mine() {
        let text = r#"{"error":"invalid_request","message":"Could not find referenced Issue.","status":400}"#;
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "result": { "isError": true, "content": [{ "type": "text", "text": text }] } });
        let e = tool_result("client", "get_issue", &msg.to_string()).unwrap_err();
        assert!(not_here(&e));
        assert!(format!("{e:#}").contains("get_issue"));
        let other = json!({ "jsonrpc": "2.0", "id": 1, "result": { "isError": true, "content": [{ "type": "text", "text": "rate limited" }] } });
        assert!(!not_here(&tool_result("client", "get_issue", &other.to_string()).unwrap_err()));
        assert!(!not_here(&anyhow!("renewing failed: refresh token not found")));
    }

    #[test]
    fn an_issue_reads_out_of_the_mcp_shape() {
        let v = json!({
            "id": "ABC-94", "uuid": "u-94", "title": "MAC changes every boot", "url": "https://linear.app/c/issue/ABC-94",
            "gitBranchName": "me/abc-94-mac", "status": "Todo", "statusType": "unstarted",
            "teamId": "t1", "projectId": "p1", "project": "V2 Hardware"
        });
        let i = issue_from(&acct(), &v, 7).unwrap();
        assert_eq!((i.key.as_str(), i.id.as_str(), i.account.as_str()), ("ABC-94", "u-94", "client"));
        assert_eq!((i.state_name.as_str(), i.state_type.as_str(), i.team_id.as_str()), ("Todo", "unstarted", "t1"));
        assert_eq!((i.project_id.as_str(), i.branch_name.as_str(), i.refreshed_ms), ("p1", "me/abc-94-mac", 7));
        assert!(issue_from(&acct(), &json!("Could not find"), 7).is_none());
    }

    #[test]
    fn a_listed_comment_names_its_thread_however_the_server_spells_it() {
        assert_eq!(parent_of(&json!({"parentId": "c1"})), json!("c1"));
        assert_eq!(parent_of(&json!({"parent": {"id": "c1"}})), json!("c1"));
        assert_eq!(parent_of(&json!({"parent": "c1"})), json!("c1"));
        assert_eq!(parent_of(&json!({"parentId": null})), Value::Null);
        let c = json!({"comments": [{"id": "c2", "parentId": "c1", "author": {"name": "G"}, "createdAt": "2", "body": "r"}]});
        let v = detail_from(&acct(), &json!({"id": "ABC-1"}), &c);
        assert_eq!((v["comments"][0]["id"].as_str(), v["comments"][0]["parent"].as_str()), (Some("c2"), Some("c1")));
    }

    #[test]
    fn states_come_in_board_order_and_a_name_finds_its_id() {
        let v = json!([
            {"id": "c", "type": "canceled", "name": "Canceled"},
            {"id": "s2", "type": "started", "name": "In Review"},
            {"id": "u", "type": "unstarted", "name": "Todo"},
            {"id": "s1", "type": "started", "name": "In Progress"},
            {"id": "b", "type": "backlog", "name": "Backlog"},
        ]);
        let states = in_board_order(v);
        let ids: Vec<&str> = states.iter().filter_map(|s| s["id"].as_str()).collect();
        assert_eq!(ids, ["b", "u", "s2", "s1", "c"]);
        assert_eq!(state_id_named(&states, "In Progress"), "s1");
        assert_eq!(state_id_named(&states, "Gone"), "");
        assert_eq!(crate::linear::start_state(&states, "unstarted").as_deref(), Some("s1"));
        assert!(in_board_order(json!({"error": "x"})).is_empty());
    }

    #[test]
    fn assigned_rows_leave_out_closed_issues_and_carry_the_cursor() {
        let v = json!({
            "issues": [
                {"id": "ABC-1", "title": "Open", "url": "u1", "status": "Todo", "statusType": "unstarted", "updatedAt": "2026-10-30"},
                {"id": "ABC-2", "title": "Done", "url": "u2", "status": "Done", "statusType": "completed"},
                {"id": "ABC-3", "title": "Dup", "url": "u3", "status": "Duplicate", "statusType": "duplicate"},
                {"id": "ABC-4", "title": "No", "url": "u4", "status": "Canceled", "statusType": "canceled"},
            ],
            "hasNextPage": true, "cursor": "c2"
        });
        let rows = open_rows(&acct(), &v);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0]["key"].as_str(), rows[0]["state"].as_str(), rows[0]["workspace"].as_str()), (Some("ABC-1"), Some("Todo"), Some("client")));
        assert_eq!(next_cursor(&v).as_deref(), Some("c2"));
        assert_eq!(next_cursor(&json!({"hasNextPage": false, "cursor": "c3"})), None);
    }

    #[test]
    fn a_scope_becomes_the_servers_team_or_project_argument() {
        let team = LinearBinding { team_id: "t1".into(), ..Default::default() };
        let project = LinearBinding { team_id: "t1".into(), project_id: "p1".into(), ..Default::default() };
        let a = scoped(json!({}), Some(&team), Some("c".into()));
        assert_eq!((a["team"].as_str(), a["project"].as_str(), a["cursor"].as_str()), (Some("t1"), None, Some("c")));
        let a = scoped(json!({}), Some(&project), None);
        assert_eq!((a["team"].as_str(), a["project"].as_str(), a.get("cursor")), (None, Some("p1"), None));
    }

    #[test]
    fn a_detail_reads_comments_oldest_first_and_the_team_from_the_key() {
        let i = json!({
            "id": "ABC-94", "title": "T", "url": "u", "description": "D", "priority": {"value": 0, "name": "No priority"},
            "status": "Todo", "project": "V2", "assignee": "me", "labels": ["Bug"], "updatedAt": "2026-10-28"
        });
        let c = json!({ "comments": [
            {"body": "second", "createdAt": "2026-10-02", "author": {"name": "Rob"}},
            {"body": "first", "createdAt": "2026-10-01", "author": {"name": "Ana"}},
        ]});
        let d = detail_from(&acct(), &i, &c);
        assert_eq!((d["key"].as_str(), d["team"].as_str(), d["priority"].as_str()), (Some("ABC-94"), Some("ABC"), Some("No priority")));
        assert_eq!((d["comments"][0]["body"].as_str(), d["comments"][0]["author"].as_str()), (Some("first"), Some("Ana")));
        assert_eq!(d["labels"][0], "Bug");
    }

    #[test]
    fn a_project_row_carries_its_uuid_not_its_identifier() {
        let v = json!({ "projects": [{ "id": "P-ABC-15", "uuid": "p-uuid", "name": "V2 Hardware",
            "status": {"name": "In Progress", "type": "started"}, "teams": [{"id": "t1", "key": "ABC", "name": "Team"}] }] });
        let rows = project_rows(&acct(), &v);
        assert_eq!((rows[0]["id"].as_str(), rows[0]["state"].as_str(), rows[0]["teams"][0]["key"].as_str()), (Some("p-uuid"), Some("started"), Some("ABC")));
    }

    #[test]
    fn a_sign_in_url_carries_pkce_and_the_resource() {
        let (verifier, challenge) = pkce().unwrap();
        assert_eq!(verifier.len(), 64);
        assert_eq!(challenge.len(), 43);
        assert!(!challenge.contains('='));
        let u = reqwest::Url::parse(&authorize_url("cid", "http://127.0.0.1:4891/oauth/linear/callback", &challenge, "st")).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["code_challenge"], challenge);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:4891/oauth/linear/callback");
        assert_eq!(q["resource"], "https://mcp.linear.app/mcp");
        assert_eq!(q["state"], "st");
    }

    #[test]
    fn a_form_body_is_url_encoded() {
        assert_eq!(form_body(&[("a", "b c"), ("redirect_uri", "http://127.0.0.1:1/cb?x=1&y")]), "a=b+c&redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcb%3Fx%3D1%26y");
    }

    /// RFC 7636 appendix B's own example.
    #[test]
    fn the_challenge_is_rfc_7636s_s256() {
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
