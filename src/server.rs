//! The desk's HTTP surface, for three callers: the plugin's hooks,
//! `embarch-ui` (which proxies the Desk tab's calls) and the desk's own MCP
//! server. Every route but `/health` wants the desk token as a bearer; the
//! browser never reaches this port directly, so a page on some other site
//! cannot make the desk post to Slack as the user.
//!
//! Writes come in two kinds (proposal decision 15): a status move goes
//! straight through — an issue's status, a tile's ✓ — and anything carrying
//! prose is a draft the user sees first. Claude drafts; nothing here sends a
//! draft on its own.

use crate::claude;
use crate::connectors::Connectors;
use crate::db::Db;
use crate::events::{self, HookEvent};
use crate::store::{self, Days, HarvestTarget, LocalDays, PendingLaunch, RealGit, State, TileStatus};
use crate::{github, harvest, linear, localrepo, slack, uri};
use axum::extract::{Path, State as Ax};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct App {
    pub db: Db,
    pub token: String,
    pub cache: Mutex<Cache>,
    pub wsl_distro: Option<String>,
    pub window_days: i64,
    pub model: String,
}

#[derive(Default)]
pub struct Cache {
    pub connectors: Vec<Value>,
    pub slack: Vec<slack::FeedItem>,
    pub slack_errors: Vec<String>,
    pub harvest_targets: Option<(i64, Vec<harvest::Target>)>,
    pub harvest_recent: Option<(i64, Vec<Value>)>,
    pub pr_checked: HashMap<String, i64>,
    pub last_status_ms: i64,
    pub last_slack_ms: i64,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}"))
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn missing(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

type ApiResult = Result<Json<Value>, ApiError>;
type Shared = Arc<App>;

pub fn router(app: Shared) -> Router {
    let authed = Router::new()
        .route("/hook", post(hook))
        .route("/view", get(view))
        .route("/items", post(create_item))
        .route("/items/{id}/pin", post(pin_item))
        .route("/items/{id}/archive", post(archive_item))
        .route("/items/{id}/link", post(link_item))
        .route("/items/{id}/states", get(item_states))
        .route("/items/{id}/status", post(item_status))
        .route("/items/{id}/launch", post(launch_item))
        .route("/sessions/{sid}", get(get_session))
        .route("/sessions/{sid}/assign", post(assign_session))
        .route("/sessions/{sid}/line", post(session_line))
        .route("/tiles/{id}", axum::routing::patch(patch_tile))
        .route("/tiles/{id}/line", post(tile_line))
        .route("/tiles/{id}/polish", post(polish_tile))
        .route("/tiles/{id}/approve", post(approve_tile))
        .route("/tiles/{id}/discard", post(discard_tile))
        .route("/harvest/targets", get(harvest_targets))
        .route("/slack/draft", post(slack_draft))
        .route("/slack/reply", post(slack_reply))
        .route("/connectors", get(list_connectors).post(add_connector))
        .route("/connectors/{kind}/{label}", delete(remove_connector))
        .route("/ignore", get(get_ignore).put(put_ignore))
        .route("/linear/issue/{key}", get(linear_issue))
        .route("/linear/mine", get(linear_mine))
        .route("/linear/search", post(linear_search))
        .route("/linear/status", post(linear_status))
        .route("/drafts", post(create_draft))
        .route("/drafts/{id}", axum::routing::patch(patch_draft))
        .route("/drafts/{id}/send", post(send_draft))
        .route("/drafts/{id}/discard", post(discard_draft))
        .route("/harvest/entries", post(harvest_entries))
        .route("/timer/start", post(timer_start))
        .route("/timer/stop", post(timer_stop))
        .route("/desks", get(list_desks).post(put_desk))
        .route("/desks/{id}", delete(delete_desk))
        .route("/open", get(get_open).post(set_open))
        .route("/day/{date}", get(day))
        .route("/linear/projects", get(linear_projects))
        .route("/github/repos", get(github_repos))
        .route("/sync", post(sync_now))
        .route_layer(axum::middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true, "service": "embarch-desk", "version": env!("CARGO_PKG_VERSION") })) }))
        .merge(authed)
        .with_state(app)
}

async fn auth(
    Ax(app): Ax<Shared>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == app.token);
    if !ok {
        return ApiError(StatusCode::UNAUTHORIZED, "missing or wrong desk token".into()).into_response();
    }
    next.run(req).await
}

// ---------------------------------------------------------------- the view

fn visible(st: &State, item: &store::WorkItem, now: i64, window_days: i64, open_tiles: &[(String, String)]) -> bool {
    !item.archived
        && (item.pinned
            || now - item.last_activity_ms < window_days * 86_400_000
            || open_tiles.iter().any(|(i, d)| i == &item.id && st.tile_seconds(i, d) > 0))
}

fn tile_json(st: &State, item_id: &str, date: &str) -> Value {
    let id = format!("{item_id}@{date}");
    let seconds = st.tile_seconds(item_id, date);
    let t = st.tiles.get(&id);
    let computed = (seconds as f64 / 3600.0 * 100.0).round() / 100.0;
    json!({
        "id": id,
        "date": date,
        "seconds": seconds,
        "hours": t.and_then(|t| t.hours_override).unwrap_or(computed),
        "computed_hours": computed,
        "lines": t.map(|t| t.lines.clone()).unwrap_or_default(),
        "note": t.and_then(|t| t.note.clone()),
        "harvest": st.tile_target(item_id, date).0,
        "harvest_from_desk": st.tile_target(item_id, date).1,
        "status": t.map(|t| t.status.clone()).unwrap_or_default(),
    })
}

fn open_tile_keys(st: &State) -> Vec<(String, String)> {
    st.tile_keys()
        .into_iter()
        .filter(|(i, d)| st.tiles.get(&format!("{i}@{d}")).is_none_or(|t| t.status == TileStatus::Open))
        .collect()
}

pub fn view_json(st: &State, cache: &Cache, now: i64, window_days: i64, days: &dyn Days) -> Value {
    let today = days.date(now);
    let open = open_tile_keys(st);
    let in_open = |i: &store::WorkItem| st.open_desk.is_none() || i.desk_id == st.open_desk;
    let mut items: Vec<&store::WorkItem> =
        st.items.values().filter(|i| visible(st, i, now, window_days, &open) && in_open(i)).collect();
    // Other desks' unsent tiles, so nothing waits unseen behind a switch.
    let elsewhere: Vec<Value> = open
        .iter()
        .filter_map(|(i, d)| {
            let item = st.items.get(i)?;
            (!in_open(item) && st.tile_seconds(i, d) > 0).then(|| {
                let mut t = tile_json(st, i, d);
                t["item_title"] = json!(item.title);
                t["desk"] = json!(item.desk_id.as_ref().and_then(|k| st.desks.get(k)).map(|k| k.name.clone()));
                t
            })
        })
        .collect();
    items.sort_by_key(|i| (!i.pinned, std::cmp::Reverse(i.last_activity_ms)));
    let cards: Vec<Value> = items
        .iter()
        .map(|item| {
            let mut tiles: Vec<Value> = open
                .iter()
                .filter(|(i, d)| i == &item.id && (d == &today || st.tile_seconds(i, d) > 0 || st.tiles.contains_key(&format!("{i}@{d}"))))
                .map(|(i, d)| tile_json(st, i, d))
                .collect();
            tiles.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str()));
            let sessions: Vec<&store::Session> = st
                .sessions
                .values()
                .filter(|s| !s.ignored && !State::is_timer_session(&s.id) && s.item_id.as_deref() == Some(&item.id))
                .collect();
            let live = sessions.iter().filter(|s| !s.ended && now - s.last_ms < 30 * 60_000).count();
            let today_sessions = sessions.iter().filter(|s| days.date(s.last_ms) == today).count();
            let mut bench: std::collections::BTreeMap<String, (u32, u32)> = Default::default();
            for b in item.bench.iter().filter(|b| now - b.ms < window_days * 86_400_000) {
                let e = bench.entry(b.tool.clone()).or_default();
                e.0 += 1;
                if !b.ok {
                    e.1 += 1;
                }
            }
            json!({
                "id": item.id,
                "title": item.title,
                "issue": item.issue,
                "repo_path": item.repo_path,
                "prs": item.prs,
                "pinned": item.pinned,
                "last_activity_ms": item.last_activity_ms,
                "sessions": { "live": live, "today": today_sessions, "total": sessions.len() },
                "bench": bench.iter().map(|(t, (n, f))| json!({ "tool": t, "count": n, "failed": f })).collect::<Vec<_>>(),
                "tiles": tiles,
            })
        })
        .collect();
    let unread = cache.slack.iter().filter(|f| f.unread).count();
    let waiting = st
        .sessions
        .values()
        .filter(|s| !s.ignored && !s.ended && s.item_id.is_none() && !State::is_timer_session(&s.id) && now - s.last_ms < 30 * 60_000)
        .count();
    json!({
        "now_ms": now,
        "today": today,
        "window_days": window_days,
        "cards": cards,
        "sessions_waiting": waiting,
        "slack": {
            "mentions": cache.slack.iter().filter(|f| f.kind == "mention").count(),
            "dms": cache.slack.iter().filter(|f| f.kind == "dm").count(),
            "unread": unread,
            "items": cache.slack,
            "errors": cache.slack_errors,
        },
        "connectors": cache.connectors,
        "ignore": st.ignore,
        "drafts": st.drafts.values().filter(|d| d.status == "open").collect::<Vec<_>>(),
        "elsewhere": elsewhere,
        "desks": st.desks.values().collect::<Vec<_>>(),
        "open_desk": st.open_desk.as_ref().and_then(|k| st.desks.get(k)),
        "day": day_json(st, &today, now),
        "timer": st.timer.as_ref().map(|t| json!({
            "item_id": t.item_id,
            "title": st.items.get(&t.item_id).map(|i| i.issue.as_ref().map(|r| format!("{} {}", r.key, r.title)).unwrap_or(i.title.clone())),
            "started_ms": t.started_ms,
            "seconds": (now - t.started_ms) / 1000,
        })),
    })
}

async fn view(Ax(app): Ax<Shared>) -> ApiResult {
    let now = now_ms();
    app.db.write(|st| st.tick_timer(now, &LocalDays));
    let cache = app.cache.lock().expect("cache");
    Ok(Json(app.db.read(|st| view_json(st, &cache, now, app.window_days, &LocalDays))))
}

// ---------------------------------------------------------------- hooks

async fn hook(Ax(app): Ax<Shared>, Json(ev): Json<HookEvent>) -> ApiResult {
    let now = now_ms();
    let ctx = app.db.write(|st| events::apply(st, &ev, now, &LocalDays, &RealGit, app.window_days));
    Ok(Json(json!({ "context": ctx })))
}

// ---------------------------------------------------------------- items

#[derive(Deserialize)]
struct NewItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    issue_key: Option<String>,
    #[serde(default)]
    desk: Option<String>,
}

/// Make (or find) the work item for an issue key, or a standalone one.
async fn ensure_item(app: &App, title: Option<&str>, issue_key: Option<&str>, desk: Option<&str>) -> Result<String, ApiError> {
    let now = now_ms();
    if let Some(key) = issue_key.map(str::trim).filter(|k| !k.is_empty()) {
        let key = key.to_ascii_uppercase();
        if let Some(id) = app.db.read(|st| st.item_by_key(&key).map(|i| i.id.clone())) {
            return Ok(id);
        }
        let conns = Connectors::load()?;
        if conns.linear.is_empty() {
            return Err(bad("no Linear workspace is connected; add a Linear API key on the Desk tab first"));
        }
        let issue = linear::find_issue(&conns.linear, &key, now)
            .await?
            .ok_or_else(|| missing(format!("no connected Linear workspace has {key}")))?;
        let title = issue.title.clone();
        return Ok(app.db.write(|st| st.create_item(&title, Some(issue), now)));
    }
    let title = title.map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| bad("give an issue key or a title"))?;
    // A standalone item's desk is asked when it is made (decision 11).
    let desk_id = app.db.read(|st| {
        if st.desks.is_empty() {
            return Ok(None);
        }
        match desk.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) if d.eq_ignore_ascii_case("none") => Ok(None),
            Some(d) => st.desk_by_name_or_id(d).map(|k| Some(k.id.clone())).ok_or_else(|| missing(format!("no desk {d:?}"))),
            None => {
                let names: Vec<String> = st.desks.values().map(|k| format!("{} ({})", k.name, k.id)).collect();
                Err(bad(format!(
                    "a standalone work item needs its desk — ask the user which: {}, or none",
                    names.join(", ")
                )))
            }
        }
    })?;
    Ok(app.db.write(|st| {
        let id = st.create_item(title, None, now);
        if let Some(i) = st.items.get_mut(&id) {
            i.desk_id = desk_id;
        }
        id
    }))
}

async fn create_item(Ax(app): Ax<Shared>, Json(b): Json<NewItem>) -> ApiResult {
    let id = ensure_item(&app, b.title.as_deref(), b.issue_key.as_deref(), b.desk.as_deref()).await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct Flag {
    value: bool,
}

async fn pin_item(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<Flag>) -> ApiResult {
    app.db
        .write(|st| st.items.get_mut(&id).map(|i| i.pinned = b.value))
        .ok_or_else(|| missing("no such work item"))?;
    Ok(Json(json!({ "ok": true })))
}

/// Archive is the user's alone (proposal decision 5): no agent tool calls it.
async fn archive_item(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<Flag>) -> ApiResult {
    app.db
        .write(|st| st.items.get_mut(&id).map(|i| i.archived = b.value))
        .ok_or_else(|| missing("no such work item"))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Link {
    #[serde(default)]
    issue_key: Option<String>,
    #[serde(default)]
    repo_path: Option<String>,
}

async fn link_item(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<Link>) -> ApiResult {
    if !app.db.read(|st| st.items.contains_key(&id)) {
        return Err(missing("no such work item"));
    }
    if let Some(path) = b.repo_path.map(|p| p.trim().trim_end_matches('/').to_string()).filter(|p| !p.is_empty()) {
        if !std::path::Path::new(&path).is_dir() {
            return Err(bad(format!("{path} is not a folder on this machine")));
        }
        app.db.write(|st| st.items.get_mut(&id).map(|i| i.repo_path = Some(path)));
    }
    if let Some(key) = b.issue_key.filter(|k| !k.trim().is_empty()) {
        let key = key.trim().to_ascii_uppercase();
        let conns = Connectors::load()?;
        let issue = linear::find_issue(&conns.linear, &key, now_ms())
            .await?
            .ok_or_else(|| missing(format!("no connected Linear workspace has {key}")))?;
        app.db.write(|st| {
            if let Some(i) = st.items.get_mut(&id) {
                i.title = issue.title.clone();
                i.issue = Some(issue);
            }
        });
    }
    Ok(Json(json!({ "ok": true })))
}

fn item_issue(app: &App, id: &str) -> Result<store::IssueRef, ApiError> {
    app.db
        .read(|st| st.items.get(id).map(|i| i.issue.clone()))
        .ok_or_else(|| missing("no such work item"))?
        .ok_or_else(|| bad("this work item has no Linear issue"))
}

async fn item_states(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let issue = item_issue(&app, &id)?;
    let conns = Connectors::load()?;
    Ok(Json(json!({ "states": linear::states(&conns.linear, &issue).await? })))
}

#[derive(Deserialize)]
struct SetState {
    state_id: String,
}

/// A status move: straight through, no draft.
async fn item_status(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<SetState>) -> ApiResult {
    let issue = item_issue(&app, &id)?;
    let conns = Connectors::load()?;
    let now = now_ms();
    let updated = linear::set_state(&conns.linear, &issue, &b.state_id, now).await?;
    app.db.write(|st| {
        if let Some(i) = st.items.get_mut(&id) {
            i.issue = Some(updated.clone());
            i.last_activity_ms = now;
        }
    });
    Ok(Json(json!({ "issue": updated })))
}

/// "Open in VS Code": two links for the browser to follow, and a claim the
/// next session starting in the repo takes.
async fn launch_item(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let now = now_ms();
    let (repo, prompt) = app
        .db
        .read(|st| {
            st.items.get(&id).map(|i| {
                let what = match &i.issue {
                    Some(r) => format!("{}: {} ({})", r.key, r.title, r.url),
                    None => i.title.clone(),
                };
                let prs: Vec<String> = i.prs.iter().filter(|p| p.state == "open").map(|p| p.url.clone()).collect();
                let prs = if prs.is_empty() { String::new() } else { format!(" Open PR: {}.", prs.join(", ")) };
                (i.repo_path.clone(), format!("Work on {what}.{prs} (EmbArch desk work item {}.)", i.id))
            })
        })
        .ok_or_else(|| missing("no such work item"))?;
    let repo = repo.ok_or_else(|| ApiError(StatusCode::CONFLICT, "this work item has no repo yet; pick its folder first".into()))?;
    app.db.write(|st| {
        st.pending_launches.retain(|p| p.item_id != id);
        st.pending_launches.push(PendingLaunch { item_id: id.clone(), repo: repo.clone(), expires_ms: now + store::LAUNCH_CLAIM_MS });
    });
    Ok(Json(json!({
        "folder_uri": uri::folder_uri(&repo, app.wsl_distro.as_deref()),
        "claude_uri": uri::claude_uri(&prompt),
        "repo": repo,
    })))
}

// ---------------------------------------------------------------- sessions

async fn get_session(Ax(app): Ax<Shared>, Path(sid): Path<String>) -> ApiResult {
    let now = now_ms();
    let today = LocalDays.date(now);
    let v = app.db.read(|st| {
        st.sessions.get(&sid).map(|s| {
            let item = s.item_id.as_ref().and_then(|i| st.items.get(i));
            json!({
                "session_id": s.id,
                "cwd": s.cwd,
                "untracked": s.ignored,
                "work_item": item.map(|i| json!({ "id": i.id, "title": i.title, "issue": i.issue, "repo_path": i.repo_path })),
                "today_tile": s.item_id.as_ref().map(|i| tile_json(st, i, &today)),
            })
        })
    });
    v.map(Json).ok_or_else(|| missing("the desk has not seen this session; is the plugin installed?"))
}

#[derive(Deserialize)]
struct Assign {
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    issue_key: Option<String>,
    #[serde(default)]
    new_title: Option<String>,
    #[serde(default)]
    desk: Option<String>,
    #[serde(default)]
    untracked: bool,
}

async fn assign_session(Ax(app): Ax<Shared>, Path(sid): Path<String>, Json(b): Json<Assign>) -> ApiResult {
    let now = now_ms();
    app.db.write(|st| {
        st.sessions.entry(sid.clone()).or_insert_with(|| store::Session {
            id: sid.clone(),
            cwd: String::new(),
            desk_id: None,
            item_id: None,
            ignored: false,
            started_ms: now,
            last_ms: now,
            spans: vec![],
            commits: vec![],
            held_lines: vec![],
            held_bench: vec![],
            ended: false,
        });
    });
    if b.untracked {
        app.db.write(|st| st.untrack_session(&sid));
        return Ok(Json(json!({ "untracked": true })));
    }
    let item_id = match b.item_id.filter(|i| !i.is_empty()) {
        Some(i) => i,
        None => ensure_item(&app, b.new_title.as_deref(), b.issue_key.as_deref(), b.desk.as_deref()).await?,
    };
    let ok = app.db.write(|st| st.assign_session(&sid, &item_id, now, &LocalDays, &RealGit));
    if !ok {
        return Err(missing(format!("no work item {item_id}")));
    }
    let title = app.db.read(|st| st.items.get(&item_id).map(|i| i.title.clone()).unwrap_or_default());
    Ok(Json(json!({ "item_id": item_id, "title": title })))
}

#[derive(Deserialize)]
struct LineBody {
    text: String,
}

async fn session_line(Ax(app): Ax<Shared>, Path(sid): Path<String>, Json(b): Json<LineBody>) -> ApiResult {
    let now = now_ms();
    let today = LocalDays.date(now);
    let landed = app.db.write(|st| {
        let sess = st.sessions.get_mut(&sid)?;
        match sess.item_id.clone() {
            Some(item) => {
                st.add_line(&item, &today, &b.text, "agent", now);
                Some(json!({ "tile": format!("{item}@{today}") }))
            }
            None => {
                sess.held_lines.push(store::Line { ms: now, text: b.text.trim().to_string(), source: "agent".into() });
                Some(json!({ "held": true, "note": "the session has no work item yet; the line lands when it gets one" }))
            }
        }
    });
    landed.map(Json).ok_or_else(|| missing("the desk has not seen this session"))
}

// ---------------------------------------------------------------- tiles

fn split_tile_id(id: &str) -> Result<(String, String), ApiError> {
    id.rsplit_once('@')
        .map(|(i, d)| (i.to_string(), d.to_string()))
        .ok_or_else(|| bad("a tile id is <item>@<date>"))
}

fn require_open(st: &State, id: &str) -> Result<(), ApiError> {
    match st.tiles.get(id).map(|t| t.status.clone()) {
        Some(TileStatus::Approved) => Err(bad("this tile is already in Harvest")),
        Some(TileStatus::Discarded) => Err(bad("this tile was discarded")),
        _ => Ok(()),
    }
}

async fn tile_line(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<LineBody>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    app.db.write(|st| {
        require_open(st, &id)?;
        st.add_line(&item, &date, &b.text, "user", now_ms());
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(app.db.read(|st| tile_json(st, &item, &date))))
}

#[derive(Deserialize)]
struct TilePatch {
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    hours: Option<f64>,
    #[serde(default)]
    harvest: Option<HarvestTarget>,
}

async fn patch_tile(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<TilePatch>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    if b.hours.is_some_and(|h| !(0.0..=24.0).contains(&h)) {
        return Err(bad("hours must be between 0 and 24"));
    }
    app.db.write(|st| {
        require_open(st, &id)?;
        if !st.items.contains_key(&item) {
            return Err(missing("no such work item"));
        }
        let t = st.tile_mut(&item, &date);
        if let Some(n) = b.note {
            t.note = Some(n);
        }
        if let Some(h) = b.hours {
            t.hours_override = Some(h);
        }
        if let Some(h) = b.harvest {
            t.harvest = Some(h);
        }
        Ok(())
    })?;
    Ok(Json(app.db.read(|st| tile_json(st, &item, &date))))
}

async fn cached_targets(app: &App) -> Result<Vec<harvest::Target>, ApiError> {
    let now = now_ms();
    if let Some((at, t)) = &app.cache.lock().expect("cache").harvest_targets {
        if now - at < 10 * 60_000 {
            return Ok(t.clone());
        }
    }
    let conns = Connectors::load()?;
    if conns.harvest.is_empty() {
        return Err(bad("no Harvest account is connected; add a Harvest token on the Desk tab first"));
    }
    let t = harvest::targets(&conns.harvest).await?;
    app.cache.lock().expect("cache").harvest_targets = Some((now, t.clone()));
    Ok(t)
}

async fn cached_recent(app: &App) -> Vec<Value> {
    let now = now_ms();
    if let Some((at, r)) = &app.cache.lock().expect("cache").harvest_recent {
        if now - at < 10 * 60_000 {
            return r.clone();
        }
    }
    let Ok(conns) = Connectors::load() else { return vec![] };
    let r = harvest::recent_entries(&conns.harvest, 45).await.unwrap_or_default();
    app.cache.lock().expect("cache").harvest_recent = Some((now, r.clone()));
    r
}

async fn harvest_targets(Ax(app): Ax<Shared>) -> ApiResult {
    Ok(Json(json!({ "targets": cached_targets(&app).await? })))
}

pub fn polish_prompt(item_label: &str, date: &str, hours: f64, lines: &[store::Line], targets: &[harvest::Target], recent: &[Value]) -> String {
    let mut p = format!("Work item: {item_label}\nDate: {date}, about {hours:.2} hours.\n\nWhat was done, from the sessions' lines (commits and milestones):\n");
    if lines.is_empty() {
        p.push_str("- (no lines were recorded; write from the work item alone)\n");
    }
    for l in lines.iter().take(60) {
        p.push_str(&format!("- [{}] {}\n", l.source, l.text));
    }
    if targets.is_empty() {
        p.push_str(
            "\nWrite `note`: one or two plain sentences in the past tense saying what was done, the way a \
             consultant writes a timesheet line — concrete, no filler, no hype. Start with the issue key \
             when there is one. Reply with exactly one JSON object: {\"note\": \"...\"}",
        );
        return p;
    }
    p.push_str("\nHarvest projects and tasks this person can log to (account_id / project_id / task_id: client / project / task):\n");
    for t in targets.iter().take(80) {
        p.push_str(&format!(
            "- {} / {} / {}: {} / {} / {}\n",
            t.account_id, t.project_id, t.task_id, t.client_name, t.project_name, t.task_name
        ));
    }
    if !recent.is_empty() {
        p.push_str("\nTheir recent entries, for how similar work was filed:\n");
        for e in recent.iter().take(40) {
            p.push_str(&format!(
                "- {} / {} / {}: {}\n",
                e["account_id"], e["project_id"], e["task_id"],
                e["notes"].as_str().unwrap_or("").chars().take(140).collect::<String>()
            ));
        }
    }
    p.push_str(
        "\nWrite `note`: one or two plain sentences in the past tense saying what was done, the way a \
         consultant writes a timesheet line — concrete, no filler, no hype. Start with the issue key \
         when there is one. Pick the account_id, project_id and task_id the time belongs to, from the \
         list only. Reply with exactly one JSON object: \
         {\"note\": \"...\", \"account_id\": 0, \"project_id\": 0, \"task_id\": 0}",
    );
    p
}

/// Opening a tile turns its lines into one Harvest note and guesses where
/// the time goes — one `claude -p` call per tile (proposal decision 13).
async fn polish_tile(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    let (label, hours, lines) = app.db.read(|st| {
        require_open(st, &id)?;
        let w = st.items.get(&item).ok_or_else(|| missing("no such work item"))?;
        let label = match &w.issue {
            Some(r) => format!("{} {} (Linear issue)", r.key, r.title),
            None => format!("{} (no Linear issue)", w.title),
        };
        let t = tile_json(st, &item, &date);
        let lines = st.tiles.get(&id).map(|t| t.lines.clone()).unwrap_or_default();
        Ok::<_, ApiError>((label, t["hours"].as_f64().unwrap_or(0.0), lines))
    })?;
    let (desk_target, _) = app.db.read(|st| st.tile_target(&item, &date));
    // A desk names its Harvest project and task (decision 10 of the open-desk
    // proposal): Claude drafts only the note, and no targets are fetched.
    let (targets, recent) = if desk_target.is_some() {
        (vec![], vec![])
    } else {
        (cached_targets(&app).await?, cached_recent(&app).await)
    };
    let prompt = polish_prompt(&label, &date, hours, &lines, &targets, &recent);
    let reply = claude::ask(
        "You write Harvest timesheet notes and choose the Harvest project and task time belongs to. Reply with one JSON object and nothing else.",
        &prompt,
        &app.model,
    )
    .await?;
    let v = claude::first_json_object(&reply).ok_or_else(|| ApiError(StatusCode::BAD_GATEWAY, format!("Claude replied without JSON: {reply}")))?;
    let note = v["note"].as_str().unwrap_or_default().trim().to_string();
    let pick = targets.iter().find(|t| {
        Some(t.account_id) == v["account_id"].as_u64()
            && Some(t.project_id) == v["project_id"].as_u64()
            && Some(t.task_id) == v["task_id"].as_u64()
    });
    app.db.write(|st| {
        let t = st.tile_mut(&item, &date);
        if !note.is_empty() {
            t.note = Some(note.clone());
        }
        if let Some(p) = pick {
            t.harvest = Some(HarvestTarget {
                account_id: p.account_id,
                project_id: p.project_id,
                task_id: p.task_id,
                label: format!("{} / {} / {}", p.client_name, p.project_name, p.task_name),
            });
        }
    });
    Ok(Json(app.db.read(|st| tile_json(st, &item, &date))))
}

/// ✓: the tile becomes a Harvest time entry. The human's alone — no MCP
/// tool reaches this route.
async fn approve_tile(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    let (target, note, hours, seconds) = app.db.read(|st| {
        require_open(st, &id)?;
        let t = st.tiles.get(&id);
        let target = st.tile_target(&item, &date).0.ok_or_else(|| bad("pick a Harvest project and task first, or Polish to have Claude guess one"))?;
        let note = t
            .and_then(|t| t.note.clone())
            .filter(|n| !n.trim().is_empty())
            .or_else(|| t.map(|t| t.lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join("; ")))
            .unwrap_or_default();
        let j = tile_json(st, &item, &date);
        Ok::<_, ApiError>((target, note, j["hours"].as_f64().unwrap_or(0.0), j["seconds"].as_i64().unwrap_or(0)))
    })?;
    if hours <= 0.0 {
        return Err(bad("this tile has no time; set its hours before sending it"));
    }
    let conns = Connectors::load()?;
    let entry = harvest::create_entry(&conns.harvest, target.account_id, target.project_id, target.task_id, &date, hours, &note).await?;
    let now = now_ms();
    app.db.write(|st| {
        let t = st.tile_mut(&item, &date);
        t.status = TileStatus::Approved;
        t.harvest_entry_id = Some(entry);
        t.acted_ms = Some(now);
        t.final_seconds = Some(seconds);
        t.note = Some(note);
        t.hours_override = Some(hours);
    });
    Ok(Json(json!({ "harvest_entry_id": entry, "hours": hours })))
}

async fn discard_tile(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    let now = now_ms();
    app.db.write(|st| {
        require_open(st, &id)?;
        let seconds = st.tile_seconds(&item, &date);
        let t = st.tile_mut(&item, &date);
        t.status = TileStatus::Discarded;
        t.acted_ms = Some(now);
        t.final_seconds = Some(seconds);
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- slack

#[derive(Deserialize)]
struct DraftBody {
    account: String,
    channel: String,
    ts: String,
}

async fn slack_draft(Ax(app): Ax<Shared>, Json(b): Json<DraftBody>) -> ApiResult {
    let msg = app
        .cache
        .lock()
        .expect("cache")
        .slack
        .iter()
        .find(|f| f.account == b.account && f.channel == b.channel && f.ts == b.ts)
        .cloned()
        .ok_or_else(|| missing("that message is no longer in the feed"))?;
    let prompt = format!(
        "{} wrote to me on Slack ({}):\n\n{}\n\nDraft my reply: short, plain, friendly, no sign-off. \
         If it needs information I would have to look up, say what I will check instead of inventing it. \
         Reply with the message text only.",
        msg.user_name,
        if msg.kind == "dm" { "a direct message".to_string() } else { format!("a mention in #{}", msg.channel_name) },
        msg.text
    );
    let draft = claude::ask("You draft short Slack replies for the user to review before sending.", &prompt, &app.model).await?;
    Ok(Json(json!({ "draft": draft.trim(), "thread_ts": msg.thread_ts.clone().unwrap_or(msg.ts.clone()) })))
}

#[derive(Deserialize)]
struct ReplyBody {
    account: String,
    channel: String,
    #[serde(default)]
    thread_ts: Option<String>,
    text: String,
}

/// Sends what the user approved — the reply's text comes from the request,
/// never from a stored draft.
async fn slack_reply(Ax(_app): Ax<Shared>, Json(b): Json<ReplyBody>) -> ApiResult {
    if b.text.trim().is_empty() {
        return Err(bad("an empty reply"));
    }
    let conns = Connectors::load()?;
    let acct = conns.slack.iter().find(|a| a.label == b.account).ok_or_else(|| missing("no such Slack connector"))?;
    let ts = slack::reply(acct, &b.channel, b.thread_ts.as_deref(), b.text.trim()).await?;
    Ok(Json(json!({ "ts": ts })))
}

// ---------------------------------------------------------------- connectors

async fn list_connectors(Ax(app): Ax<Shared>) -> ApiResult {
    let conns = Connectors::load()?;
    let labels: Value = json!({
        "linear": conns.linear.iter().map(|a| &a.label).collect::<Vec<_>>(),
        "harvest": conns.harvest.iter().map(|a| &a.label).collect::<Vec<_>>(),
        "github": conns.github.iter().map(|a| &a.label).collect::<Vec<_>>(),
        "slack": conns.slack.iter().map(|a| &a.label).collect::<Vec<_>>(),
    });
    let status = app.cache.lock().expect("cache").connectors.clone();
    Ok(Json(json!({ "accounts": labels, "status": status })))
}

#[derive(Deserialize)]
struct AddConnector {
    kind: String,
    label: String,
    token: String,
}

async fn check_one(kind: &str, acct: &crate::connectors::Account) -> Value {
    let r: anyhow::Result<String> = match kind {
        "linear" => linear::whoami(acct).await,
        "harvest" => harvest::accounts(std::slice::from_ref(acct))
            .await
            .map(|v| v.iter().map(|(_, a)| a.name.clone()).collect::<Vec<_>>().join(", ")),
        "github" => github::whoami(acct).await,
        "slack" => slack::whoami(acct).await.map(|(_, team, _)| team),
        _ => Err(anyhow::anyhow!("unknown kind")),
    };
    match r {
        Ok(detail) => json!({ "kind": kind, "label": acct.label, "ok": true, "detail": detail }),
        Err(e) => json!({ "kind": kind, "label": acct.label, "ok": false, "detail": format!("{e:#}") }),
    }
}

/// A pasted token is checked against its service before it is kept, so a
/// typo is an error now rather than a silent empty card later.
async fn add_connector(Ax(app): Ax<Shared>, Json(b): Json<AddConnector>) -> ApiResult {
    let acct = crate::connectors::Account { label: b.label.trim().into(), token: b.token.trim().into() };
    let check = check_one(&b.kind, &acct).await;
    if check["ok"].as_bool() != Some(true) {
        return Err(bad(format!("{} rejected that token: {}", b.kind, check["detail"].as_str().unwrap_or("?"))));
    }
    let mut conns = Connectors::load()?;
    conns.upsert(&b.kind, &acct.label, &acct.token).map_err(|e| bad(e.to_string()))?;
    conns.save()?;
    app.cache.lock().expect("cache").last_status_ms = 0;
    Ok(Json(check))
}

async fn remove_connector(Ax(app): Ax<Shared>, Path((kind, label)): Path<(String, String)>) -> ApiResult {
    let mut conns = Connectors::load()?;
    let removed = conns.remove(&kind, &label).map_err(|e| bad(e.to_string()))?;
    conns.save()?;
    app.cache.lock().expect("cache").last_status_ms = 0;
    Ok(Json(json!({ "removed": removed })))
}

// ---------------------------------------------------------------- ignore list

async fn get_ignore(Ax(app): Ax<Shared>) -> ApiResult {
    Ok(Json(json!({ "paths": app.db.read(|st| st.ignore.clone()) })))
}

#[derive(Deserialize)]
struct IgnoreBody {
    paths: Vec<String>,
}

async fn put_ignore(Ax(app): Ax<Shared>, Json(b): Json<IgnoreBody>) -> ApiResult {
    let paths: Vec<String> = b
        .paths
        .iter()
        .map(|p| p.trim().trim_end_matches('/').to_string())
        .filter(|p| p.starts_with('/'))
        .collect();
    app.db.write(|st| {
        st.ignore = paths.clone();
        // A session already running under a newly ignored path stops counting now.
        for s in st.sessions.values_mut() {
            if store::is_ignored(&s.cwd, &paths) && s.item_id.is_none() {
                s.ignored = true;
                s.spans.clear();
            }
        }
    });
    Ok(Json(json!({ "paths": paths })))
}

// ---------------------------------------------------------------- Linear and Harvest for agents
//
// The one access point (decisions 29-32): agents read Linear and Harvest
// through these routes instead of the hosted MCP servers, on the desk's own
// tokens, and write under the tab's rules — a status move goes through, prose
// becomes a draft, time becomes a tile line and never an entry.

fn linear_accounts() -> Result<Vec<crate::connectors::Account>, ApiError> {
    let c = Connectors::load()?;
    if c.linear.is_empty() {
        return Err(bad("no Linear workspace is connected; add a Linear API key on the Desk tab"));
    }
    Ok(c.linear)
}

async fn linear_issue(Path(key): Path<String>) -> ApiResult {
    Ok(Json(linear::read_issue(&linear_accounts()?, key.trim()).await?))
}

fn open_linear(app: &App) -> Option<store::LinearBinding> {
    app.db.read(|st| st.open_desk.as_ref().and_then(|k| st.desks.get(k)).and_then(|d| d.linear.clone()))
}

async fn linear_mine(Ax(app): Ax<Shared>) -> ApiResult {
    let scope = open_linear(&app);
    Ok(Json(json!({
        "scope": scope.as_ref().map(|b| b.project_name.clone()),
        "issues": linear::my_issues(&linear_accounts()?, scope.as_ref()).await?,
    })))
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
}

async fn linear_search(Ax(app): Ax<Shared>, Json(b): Json<SearchBody>) -> ApiResult {
    if b.query.trim().is_empty() {
        return Err(bad("an empty search"));
    }
    let scope = open_linear(&app);
    Ok(Json(json!({
        "scope": scope.as_ref().map(|b| b.project_name.clone()),
        "issues": linear::search(&linear_accounts()?, &b.query, scope.as_ref()).await?,
    })))
}

#[derive(Deserialize)]
struct StatusBody {
    key: String,
    status: String,
}

/// A status move by state name: straight through, like the card's dropdown.
async fn linear_status(Ax(app): Ax<Shared>, Json(b): Json<StatusBody>) -> ApiResult {
    let now = now_ms();
    let updated = linear::set_state_by_name(&linear_accounts()?, b.key.trim(), &b.status, now).await?;
    app.db.write(|st| {
        if let Some(id) = st.item_by_key(&updated.key).map(|i| i.id.clone()) {
            if let Some(i) = st.items.get_mut(&id) {
                i.issue = Some(updated.clone());
                i.last_activity_ms = now;
            }
        }
    });
    Ok(Json(json!({ "key": updated.key, "state": updated.state_name })))
}

#[derive(Deserialize)]
struct NewDraft {
    kind: String,
    #[serde(default)]
    issue_key: Option<String>,
    #[serde(default)]
    team_key: Option<String>,
    #[serde(default)]
    title: Option<String>,
    body: String,
    #[serde(default)]
    session_id: Option<String>,
}

fn clean(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// An agent's proposed comment or issue. Nothing is sent: it waits on the
/// Desk tab for the user's Send, as they leave it.
async fn create_draft(Ax(app): Ax<Shared>, Json(b): Json<NewDraft>) -> ApiResult {
    let now = now_ms();
    let issue_key = clean(&b.issue_key).map(|k| k.to_ascii_uppercase());
    let desk_linear = open_linear(&app);
    let named_team = clean(&b.team_key).map(|k| k.to_ascii_uppercase());
    // A new issue with no team named goes into the open desk's project.
    let project_id = if b.kind == "linear_issue" && named_team.is_none() {
        desk_linear.as_ref().map(|l| l.project_id.clone()).filter(|p| !p.is_empty())
    } else {
        None
    };
    let team_key = named_team.or_else(|| desk_linear.as_ref().filter(|_| b.kind == "linear_issue").map(|l| l.team_key.clone()).filter(|k| !k.is_empty()));
    let title = clean(&b.title);
    match b.kind.as_str() {
        "linear_comment" if issue_key.is_none() => return Err(bad("a comment draft needs issue_key")),
        "linear_issue" if team_key.is_none() || title.is_none() => {
            return Err(bad("an issue draft needs a title, and a team_key when no desk is open"))
        }
        "linear_comment" | "linear_issue" => {}
        k => return Err(bad(format!("unknown draft kind {k:?}; linear_comment or linear_issue"))),
    }
    if b.body.trim().is_empty() && b.kind == "linear_comment" {
        return Err(bad("an empty comment"));
    }
    let id = app.db.write(|st| {
        let item_id = issue_key
            .as_deref()
            .and_then(|k| st.item_by_key(k).map(|i| i.id.clone()))
            .or_else(|| b.session_id.as_deref().and_then(|s| st.sessions.get(s)).and_then(|s| s.item_id.clone()));
        let id = st.new_id("d");
        st.drafts.insert(
            id.clone(),
            store::Draft {
                id: id.clone(),
                kind: b.kind.clone(),
                issue_key,
                team_key,
                title,
                body: b.body.trim().to_string(),
                item_id,
                created_ms: now,
                status: "open".into(),
                url: None,
                project_id,
            },
        );
        id
    });
    Ok(Json(json!({ "draft_id": id, "status": "waiting for the user's Send on the Desk tab; nothing was posted" })))
}

#[derive(Deserialize)]
struct DraftPatch {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

async fn patch_draft(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<DraftPatch>) -> ApiResult {
    app.db.write(|st| {
        let d = st.drafts.get_mut(&id).filter(|d| d.status == "open").ok_or_else(|| missing("no open draft with that id"))?;
        if let Some(t) = b.title {
            d.title = Some(t);
        }
        if let Some(body) = b.body {
            d.body = body;
        }
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(json!({ "ok": true })))
}

/// The user's Send. The text sent is the draft as it stands now — edited on
/// the tab, or as the agent left it if the user left it alone.
async fn send_draft(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let d = app
        .db
        .read(|st| st.drafts.get(&id).filter(|d| d.status == "open").cloned())
        .ok_or_else(|| missing("no open draft with that id"))?;
    let accounts = linear_accounts()?;
    let url = match d.kind.as_str() {
        "linear_comment" => linear::post_comment(&accounts, d.issue_key.as_deref().unwrap_or_default(), &d.body).await?,
        _ => {
            let (key, url) = linear::create_issue(
                &accounts,
                d.team_key.as_deref().unwrap_or_default(),
                d.title.as_deref().unwrap_or_default(),
                &d.body,
                d.project_id.as_deref(),
            )
            .await?;
            // A standalone card the agent was on gains the issue it filed.
            if let Some(item) = d.item_id.clone() {
                if let Ok(Some(issue)) = linear::find_issue(&accounts, &key, now_ms()).await {
                    app.db.write(|st| {
                        if let Some(i) = st.items.get_mut(&item).filter(|i| i.issue.is_none()) {
                            i.title = issue.title.clone();
                            i.issue = Some(issue);
                        }
                    });
                }
            }
            url
        }
    };
    app.db.write(|st| {
        if let Some(x) = st.drafts.get_mut(&id) {
            x.status = "sent".into();
            x.url = Some(url.clone());
        }
    });
    Ok(Json(json!({ "url": url })))
}

async fn discard_draft(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    app.db
        .write(|st| st.drafts.get_mut(&id).filter(|d| d.status == "open").map(|d| d.status = "discarded".into()))
        .ok_or_else(|| missing("no open draft with that id"))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct EntriesBody {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

/// Harvest, read-only for agents: entries already logged. Time goes in as
/// tile lines and leaves on the user's ✓ (decision 10).
async fn harvest_entries(Json(b): Json<EntriesBody>) -> ApiResult {
    let conns = Connectors::load()?;
    if conns.harvest.is_empty() {
        return Err(bad("no Harvest account is connected"));
    }
    let today = chrono::Local::now().date_naive();
    let monday = today - chrono::Duration::days(chrono::Datelike::weekday(&today).num_days_from_monday() as i64);
    let from = b.from.unwrap_or_else(|| monday.to_string());
    let to = b.to.unwrap_or_else(|| today.to_string());
    for d in [&from, &to] {
        if chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_err() {
            return Err(bad(format!("{d} is not a YYYY-MM-DD date")));
        }
    }
    let entries = harvest::entries_between(&conns.harvest, &from, &to).await?;
    let total: f64 = entries.iter().filter_map(|e| e["hours"].as_f64()).sum();
    Ok(Json(json!({ "from": from, "to": to, "total_hours": (total * 100.0).round() / 100.0, "entries": entries })))
}

#[derive(Deserialize)]
struct TimerBody {
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    issue_key: Option<String>,
    #[serde(default)]
    new_title: Option<String>,
    #[serde(default)]
    desk: Option<String>,
}

/// The manual timer, for work away from Claude Code — what harvest:track
/// covered (decision 31).
async fn timer_start(Ax(app): Ax<Shared>, Json(b): Json<TimerBody>) -> ApiResult {
    let item_id = match clean(&b.item_id) {
        Some(i) => i,
        None => ensure_item(&app, b.new_title.as_deref(), b.issue_key.as_deref(), b.desk.as_deref()).await?,
    };
    let now = now_ms();
    if !app.db.write(|st| st.start_timer(&item_id, now, &LocalDays)) {
        return Err(missing(format!("no work item {item_id}")));
    }
    let title = app.db.read(|st| st.items.get(&item_id).map(|i| i.title.clone()).unwrap_or_default());
    Ok(Json(json!({ "item_id": item_id, "title": title, "started_ms": now })))
}

async fn timer_stop(Ax(app): Ax<Shared>) -> ApiResult {
    let now = now_ms();
    match app.db.write(|st| st.stop_timer(now, &LocalDays)) {
        Some(t) => Ok(Json(json!({ "item_id": t.item_id, "minutes": (now - t.started_ms) / 60_000 }))),
        None => Ok(Json(json!({ "stopped": false, "note": "no timer was running" }))),
    }
}

// ---------------------------------------------------------------- desks
//
// One desk per project (embarch-open-desk-proposal.md): a Linear project, a
// Harvest project and task, GitHub repos, firmware repos, a saved bench. One
// is open suite-wide, or none. Other processes read the open one from
// `open-desk.json` beside the state file, so neither embarch-api nor
// embarch-ui has to reach this service to know it.

pub fn publish_open(app: &App) {
    let body = app.db.read(|st| json!({ "desk": st.open_desk.as_ref().and_then(|k| st.desks.get(k)) }));
    let path = match crate::paths::data_dir() {
        Ok(d) => d.join("open-desk.json"),
        Err(_) => return,
    };
    if let Err(e) = crate::paths::write_private(&path, body.to_string().as_bytes()) {
        tracing::warn!("publishing the open desk failed: {e:#}");
    }
}

async fn list_desks(Ax(app): Ax<Shared>) -> ApiResult {
    Ok(Json(app.db.read(|st| json!({ "desks": st.desks.values().collect::<Vec<_>>(), "open": st.open_desk }))))
}

#[derive(Deserialize)]
struct DeskBody {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    linear: Option<store::LinearBinding>,
    #[serde(default)]
    harvest: Option<HarvestTarget>,
    #[serde(default)]
    github_repos: Vec<String>,
    #[serde(default)]
    firmware_repos: Vec<String>,
    #[serde(default)]
    bench: Option<String>,
}

/// Make or edit a desk, from the Desk tab's form.
async fn put_desk(Ax(app): Ax<Shared>, Json(b): Json<DeskBody>) -> ApiResult {
    let name = b.name.trim().to_string();
    if name.is_empty() {
        return Err(bad("a desk needs a name"));
    }
    let mut repos = vec![];
    for r in b.firmware_repos.iter().map(|r| r.trim().trim_end_matches('/')).filter(|r| !r.is_empty()) {
        let path = std::fs::canonicalize(r).map_err(|_| bad(format!("{r} is not a folder on this machine")))?;
        if !path.is_dir() {
            return Err(bad(format!("{r} is not a folder")));
        }
        repos.push(path.display().to_string());
    }
    let github: Vec<String> = b
        .github_repos
        .iter()
        .map(|r| r.trim().trim_start_matches("https://github.com/").trim_end_matches(".git").to_string())
        .filter(|r| !r.is_empty())
        .collect();
    if let Some(bad_repo) = github.iter().find(|r| r.split('/').count() != 2) {
        return Err(bad(format!("{bad_repo} is not owner/repo")));
    }
    let id = app.db.write(|st| {
        let id = b.id.clone().filter(|i| st.desks.contains_key(i)).unwrap_or_default();
        // A firmware repo belongs to at most one desk: two desks naming one
        // repo could not say whose a session there is.
        if let Some((other, repo)) = st
            .desks
            .values()
            .filter(|d| d.id != id)
            .find_map(|d| repos.iter().find(|r| d.firmware_repos.contains(r)).map(|r| (d.name.clone(), r.clone())))
        {
            return Err(bad(format!("{repo} is already the desk {other:?}'s")));
        }
        let created = st.desks.get(&id).map(|d| d.created_ms).unwrap_or_default();
        let desk = store::Desk {
            id,
            name,
            linear: b.linear.clone(),
            harvest: b.harvest.clone(),
            github_repos: github,
            firmware_repos: repos,
            bench: b.bench.clone().filter(|x| !x.trim().is_empty()),
            created_ms: created,
        };
        Ok(st.put_desk(desk, now_ms()))
    })?;
    publish_open(&app);
    Ok(Json(json!({ "id": id })))
}

async fn delete_desk(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let found = app.db.write(|st| {
        let found = st.desks.remove(&id).is_some();
        for i in st.items.values_mut().filter(|i| i.desk_id.as_deref() == Some(id.as_str())) {
            i.desk_id = None;
        }
        if st.open_desk.as_deref() == Some(id.as_str()) {
            st.open_desk = None;
        }
        found
    });
    if !found {
        return Err(missing("no such desk"));
    }
    publish_open(&app);
    Ok(Json(json!({ "ok": true })))
}

async fn get_open(Ax(app): Ax<Shared>) -> ApiResult {
    Ok(Json(app.db.read(|st| json!({ "desk": st.open_desk.as_ref().and_then(|k| st.desks.get(k)) }))))
}

#[derive(Deserialize)]
struct OpenBody {
    /// A desk's id or name; absent, null or "none" opens no desk.
    #[serde(default)]
    desk: Option<String>,
}

/// Open a desk suite-wide (or none). Touches no hardware: its bench is only
/// proposed, by the UI, and applied on the user's click.
async fn set_open(Ax(app): Ax<Shared>, Json(b): Json<OpenBody>) -> ApiResult {
    let want = b.desk.map(|d| d.trim().to_string()).filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("none"));
    let desk = app.db.write(|st| match want {
        None => {
            st.open_desk = None;
            Ok::<_, ApiError>(None)
        }
        Some(w) => {
            let d = st.desk_by_name_or_id(&w).cloned().ok_or_else(|| {
                let names: Vec<String> = st.desks.values().map(|k| k.name.clone()).collect();
                missing(format!("no desk {w:?}; there are: {}", names.join(", ")))
            })?;
            st.open_desk = Some(d.id.clone());
            Ok(Some(d))
        }
    })?;
    publish_open(&app);
    app.cache.lock().expect("cache").pr_checked.clear();
    Ok(Json(json!({ "desk": desk })))
}

async fn linear_projects() -> ApiResult {
    Ok(Json(json!({ "projects": linear::projects(&linear_accounts()?).await? })))
}

async fn github_repos() -> ApiResult {
    let conns = Connectors::load()?;
    Ok(Json(json!({ "repos": github::user_repos(&conns.github_accounts()).await? })))
}

/// One day's work, grouped by desk — the open one first — for the list under
/// the cards: every work item with time, a line, bench activity or a session
/// that day, with its links, what was done, its status and sessions.
pub fn day_json(st: &State, date: &str, now: i64) -> Value {
    let days = LocalDays;
    let mut rows: Vec<(Option<String>, Value)> = vec![];
    for item in st.items.values() {
        let seconds = st.tile_seconds(&item.id, date);
        let tile = st.tiles.get(&format!("{}@{date}", item.id));
        let sessions: Vec<&store::Session> = st
            .sessions
            .values()
            .filter(|s| !s.ignored && !State::is_timer_session(&s.id))
            .filter(|s| s.spans.iter().any(|sp| sp.date == date && sp.item_id.as_deref() == Some(item.id.as_str())))
            .collect();
        let bench: Vec<&store::BenchEvent> = item.bench.iter().filter(|b| days.date(b.ms) == date).collect();
        let lines = tile.map(|t| t.lines.clone()).unwrap_or_default();
        if seconds == 0 && lines.is_empty() && bench.is_empty() && sessions.is_empty() {
            continue;
        }
        let t = tile_json(st, &item.id, date);
        rows.push((
            item.desk_id.clone(),
            json!({
                "id": item.id,
                "title": item.title,
                "issue": item.issue,
                "prs": item.prs,
                "hours": t["hours"],
                "status": t["status"],
                "harvest": t["harvest"],
                "lines": lines,
                "note": tile.and_then(|t| t.note.clone()),
                "sessions": sessions.len(),
                "live": sessions.iter().filter(|s| !s.ended && now - s.last_ms < 30 * 60_000).count(),
                "bench": bench.len(),
                "bench_failed": bench.iter().filter(|b| !b.ok).count(),
            }),
        ));
    }
    let mut groups: Vec<(i32, String, Option<String>, Vec<Value>)> = vec![];
    for (desk, row) in rows {
        let (rank, name) = match &desk {
            Some(k) if st.open_desk.as_ref() == Some(k) => (0, st.desks.get(k).map(|d| d.name.clone()).unwrap_or_default()),
            Some(k) => (1, st.desks.get(k).map(|d| d.name.clone()).unwrap_or_else(|| k.clone())),
            None => (2, "No desk".to_string()),
        };
        match groups.iter_mut().find(|g| g.2 == desk) {
            Some(g) => g.3.push(row),
            None => groups.push((rank, name, desk, vec![row])),
        }
    }
    groups.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let total: f64 = groups.iter().flat_map(|g| g.3.iter()).filter_map(|r| r["hours"].as_f64()).sum();
    json!({
        "date": date,
        "total_hours": (total * 100.0).round() / 100.0,
        "groups": groups.into_iter().map(|(_, name, desk, items)| json!({ "desk": desk, "name": name, "items": items })).collect::<Vec<_>>(),
    })
}

async fn day(Ax(app): Ax<Shared>, Path(date): Path<String>) -> ApiResult {
    if chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
        return Err(bad(format!("{date} is not a YYYY-MM-DD date")));
    }
    let now = now_ms();
    Ok(Json(app.db.read(|st| day_json(st, &date, now))))
}

// ---------------------------------------------------------------- sync

async fn sync_now(Ax(app): Ax<Shared>) -> ApiResult {
    {
        let mut c = app.cache.lock().expect("cache");
        c.last_status_ms = 0;
        c.last_slack_ms = 0;
        c.pr_checked.clear();
    }
    sync_once(&app, true).await;
    Ok(Json(json!({ "ok": true })))
}

/// One pass of the background refresh: connector checks every five
/// minutes, the visible cards' issues and PRs, the Slack feed every two.
pub async fn sync_once(app: &App, force_issues: bool) {
    let now = now_ms();
    let Ok(conns) = Connectors::load() else { return };

    if now - app.cache.lock().expect("cache").last_status_ms > 5 * 60_000 {
        let mut status = vec![];
        for (kind, list) in [("linear", &conns.linear), ("harvest", &conns.harvest), ("slack", &conns.slack)] {
            for a in list.iter() {
                status.push(check_one(kind, a).await);
            }
        }
        for a in conns.github_accounts() {
            status.push(check_one("github", &a).await);
        }
        let mut c = app.cache.lock().expect("cache");
        c.connectors = status;
        c.last_status_ms = now;
    }

    let open = app.db.read(open_tile_keys);
    type Card = (String, Option<store::IssueRef>, bool, Vec<String>);
    let cards: Vec<Card> = app.db.read(|st| {
        st.items
            .values()
            .filter(|i| visible(st, i, now, app.window_days, &open))
            .map(|i| {
                let repos = i.desk_id.as_ref().and_then(|k| st.desks.get(k)).map(|d| d.github_repos.clone()).unwrap_or_default();
                (i.id.clone(), i.issue.clone(), i.repo_path.is_none(), repos)
            })
            .collect()
    });
    let gh = conns.github_accounts();
    for (id, issue, needs_repo, repos) in cards {
        let Some(issue) = issue else { continue };
        if force_issues || now - issue.refreshed_ms > 3 * 60_000 {
            if let Ok(Some(fresh)) = linear::refresh_issue(&conns.linear, &issue, now).await {
                app.db.write(|st| {
                    let desk = st.desk_for_issue(&fresh);
                    if let Some(i) = st.items.get_mut(&id) {
                        if i.desk_id.is_none() {
                            i.desk_id = desk;
                        }
                        i.issue = Some(fresh);
                    }
                });
            }
        }
        let due = now - app.cache.lock().expect("cache").pr_checked.get(&id).copied().unwrap_or(0) > 5 * 60_000;
        if due && !gh.is_empty() {
            if let Ok(prs) = github::prs_for(&gh, &issue.key, &repos).await {
                let clone = if needs_repo { prs.first().and_then(|p| localrepo::find_clone(&p.repo)) } else { None };
                app.db.write(|st| {
                    if let Some(i) = st.items.get_mut(&id) {
                        if prs.iter().any(|p| !i.prs.iter().any(|q| q.url == p.url)) {
                            i.last_activity_ms = now;
                        }
                        i.prs = prs;
                        if i.repo_path.is_none() {
                            i.repo_path = clone;
                        }
                    }
                });
            }
            app.cache.lock().expect("cache").pr_checked.insert(id, now);
        }
    }

    if now - app.cache.lock().expect("cache").last_slack_ms > 2 * 60_000 {
        let mut items = vec![];
        let mut errors = vec![];
        for a in &conns.slack {
            match slack::feed(a).await {
                Ok(mut f) => items.append(&mut f),
                Err(e) => errors.push(format!("{}: {e:#}", a.label)),
            }
        }
        slack::sort_feed(&mut items);
        let mut c = app.cache.lock().expect("cache");
        c.slack = items;
        c.slack_errors = errors;
        c.last_slack_ms = now;
    }

    app.db.write(|st| st.tick_timer(now, &LocalDays));
    let archived = app.db.write(|st| st.prune(now));
    if let Err(e) = app.db.archive(&archived) {
        tracing::warn!("archiving tiles failed: {e:#}");
    }
}

pub async fn sync_loop(app: Shared) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tick.tick().await;
        sync_once(&app, false).await;
    }
}

pub async fn flush_loop(app: Shared) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tick.tick().await;
        if let Err(e) = app.db.flush() {
            tracing::warn!("saving state failed: {e:#}");
        }
    }
}

/// `\\wsl.localhost\Ubuntu\` → `Ubuntu`, when the desk runs under WSL.
pub fn detect_wsl_distro() -> Option<String> {
    if let Ok(d) = std::env::var("WSL_DISTRO_NAME") {
        if !d.is_empty() {
            return Some(d);
        }
    }
    let out = std::process::Command::new("wslpath").args(["-w", "/"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().trim_start_matches('\\').split('\\').nth(1).filter(|d| !d.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::UtcDays;

    const D0: i64 = 1_792_627_200_000;
    const H: i64 = 3_600_000;

    #[test]
    fn the_view_shows_active_cards_with_their_open_tiles_and_hides_archived_ones() {
        let mut st = State::default();
        let a = st.create_item("Active", None, D0);
        let b = st.create_item("Archived", None, D0);
        st.items.get_mut(&b).unwrap().archived = true;
        let old = st.create_item("Old", None, D0 - 30 * 24 * H);
        let mut s = crate::store::testing::session("s");
        s.item_id = Some(a.clone());
        crate::store::record_activity(&mut s, D0 + 9 * H, &UtcDays);
        crate::store::record_activity(&mut s, D0 + 11 * H, &UtcDays);
        st.sessions.insert("s".into(), s);
        let v = view_json(&st, &Cache::default(), D0 + 12 * H, 7, &UtcDays);
        let cards = v["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 1, "{v}");
        assert_eq!(cards[0]["id"], a.as_str());
        assert_eq!(cards[0]["tiles"][0]["hours"], 2.0);
        assert_eq!(cards[0]["tiles"][0]["status"], "open");
        assert!(!cards.iter().any(|c| c["id"] == old.as_str()));
    }

    #[test]
    fn an_approved_tile_leaves_the_card() {
        let mut st = State::default();
        let a = st.create_item("x", None, D0);
        let t = st.tile_mut(&a, "2026-10-22");
        t.status = TileStatus::Approved;
        t.final_seconds = Some(3600);
        let v = view_json(&st, &Cache::default(), D0 + H, 7, &UtcDays);
        assert_eq!(v["cards"][0]["tiles"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn open_drafts_and_a_running_timer_reach_the_view_and_sent_drafts_do_not() {
        let mut st = State::default();
        let a = st.create_item("Bench rework", None, D0);
        for (id, status) in [("d1", "open"), ("d2", "sent")] {
            st.drafts.insert(id.into(), store::Draft {
                id: id.into(), kind: "linear_comment".into(), issue_key: Some("ABC-1".into()), team_key: None,
                title: None, body: "x".into(), item_id: None, created_ms: D0, status: status.into(), url: None,
                project_id: None,
            });
        }
        st.start_timer(&a, D0 + H, &UtcDays);
        let v = view_json(&st, &Cache::default(), D0 + 2 * H, 7, &UtcDays);
        assert_eq!(v["drafts"].as_array().unwrap().len(), 1);
        assert_eq!(v["timer"]["item_id"], a.as_str());
        assert_eq!(v["timer"]["seconds"], 3600);
        assert_eq!(v["cards"][0]["sessions"]["total"], 0, "the timer is not counted as a Claude session");
        assert_eq!(v["sessions_waiting"], 0);
    }

    #[test]
    fn an_open_desk_scopes_the_cards_and_other_desks_unsent_tiles_wait_in_a_strip() {
        let mut st = State::default();
        let a = st.put_desk(store::Desk { name: "Alpha".into(), ..Default::default() }, D0);
        let b = st.put_desk(store::Desk { name: "Beta".into(), ..Default::default() }, D0);
        let wa = st.create_item("on alpha", None, D0);
        let wb = st.create_item("on beta", None, D0);
        st.items.get_mut(&wa).unwrap().desk_id = Some(a.clone());
        st.items.get_mut(&wb).unwrap().desk_id = Some(b.clone());
        let mut s1 = crate::store::testing::session("s");
        s1.item_id = Some(wb.clone());
        crate::store::record_activity(&mut s1, D0 + 9 * H, &UtcDays);
        crate::store::record_activity(&mut s1, D0 + 10 * H, &UtcDays);
        st.sessions.insert("s".into(), s1);
        st.open_desk = Some(a.clone());
        let v = view_json(&st, &Cache::default(), D0 + 11 * H, 7, &UtcDays);
        let cards: Vec<&str> = v["cards"].as_array().unwrap().iter().map(|c| c["title"].as_str().unwrap()).collect();
        assert_eq!(cards, ["on alpha"]);
        assert_eq!(v["elsewhere"][0]["item_title"], "on beta");
        assert_eq!(v["elsewhere"][0]["desk"], "Beta");
        assert_eq!(v["open_desk"]["name"], "Alpha");
        st.open_desk = None;
        let all = view_json(&st, &Cache::default(), D0 + 11 * H, 7, &UtcDays);
        assert_eq!(all["cards"].as_array().unwrap().len(), 2, "no desk open shows every card");
    }

    #[test]
    fn the_day_list_groups_by_desk_with_the_open_one_first() {
        let mut st = State::default();
        let a = st.put_desk(store::Desk { name: "Alpha".into(), ..Default::default() }, D0);
        let b = st.put_desk(store::Desk { name: "Beta".into(), ..Default::default() }, D0);
        for (title, desk) in [("one", &a), ("two", &b)] {
            let w = st.create_item(title, None, D0);
            st.items.get_mut(&w).unwrap().desk_id = Some(desk.clone());
            st.add_line(&w, "2026-10-22", "did a thing", "agent", D0 + H);
        }
        st.open_desk = Some(b.clone());
        let d = day_json(&st, "2026-10-22", D0 + 2 * H);
        let names: Vec<&str> = d["groups"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Beta", "Alpha"]);
        assert_eq!(d["groups"][0]["items"][0]["lines"][0]["text"], "did a thing");
        assert_eq!(day_json(&st, "2026-10-21", D0)["groups"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_polish_prompt_carries_lines_targets_and_the_reply_shape() {
        let lines = vec![store::Line { ms: 0, text: "smp: retry a busy upload".into(), source: "commit".into() }];
        let targets = vec![harvest::Target {
            account_id: 1, account_name: "a".into(), project_id: 2, project_name: "Fw".into(),
            client_name: "C".into(), task_id: 3, task_name: "Dev".into(),
        }];
        let p = polish_prompt("ABC-123 Bootload", "2026-10-22", 2.5, &lines, &targets, &[]);
        assert!(p.contains("smp: retry a busy upload") && p.contains("1 / 2 / 3: C / Fw / Dev") && p.contains("\"note\""));
    }
}
