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
use crate::store::{self, Days, HarvestTarget, LocalDays, MailboxEntry, PendingLaunch, RealGit, State, TakeError, TileStatus};
use crate::{brief, github, harvest, linear, localrepo, refs, slack, uri};
use axum::extract::{Path, Query, State as Ax};
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
    /// Every desk's reference index (decision 56).
    pub refs: refs::Store,
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
    pub logged_fetched: HashMap<String, i64>,
    /// `claude agents --json`, by session id: live state of every session.
    pub agents: HashMap<String, Value>,
    /// When `agents` was read.
    pub agents_ms: i64,
    pub wsl_distro: Option<String>,
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
        .route("/refs", get(refs_route))
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
        .route("/tiles/{id}/fold", post(fold_tile))
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
        .route("/harvest/entry/{account}/{id}", axum::routing::patch(edit_entry).delete(remove_entry))
        .route("/pickup", get(pickup))
        .route("/items/{id}/start", post(start_work))
        .route("/items/{id}/brief", get(item_brief))
        .route("/items/{id}/notes", get(item_notes))
        .route("/mailbox", get(get_mailbox).post(post_mailbox))
        .route("/mailbox/{id}/take", post(take_mailbox))
        .route("/settings", get(get_settings).put(put_settings))
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

/// A tile's lines as the routes hand them out: each note under the field
/// names `/notes` uses (`id`, `ts_ms`), with `ms` kept for the clients
/// that read it (chat-desk proposal, Interface).
fn lines_json(lines: &[store::Line]) -> Vec<Value> {
    lines
        .iter()
        .map(|l| {
            let mut v = json!(l);
            v["ts_ms"] = json!(l.ms);
            v
        })
        .collect()
}

fn tile_json(st: &State, item_id: &str, date: &str) -> Value {
    let id = format!("{item_id}@{date}");
    let seconds = st.tile_seconds(item_id, date);
    let t = st.tiles.get(&id);
    let folded = t.is_some_and(|t| t.status == TileStatus::Folded);
    let captured = seconds as f64 / 3600.0;
    let logged = st.logged_hours(item_id, date);
    // The tile proposes only what Harvest doesn't already hold for this item
    // that day (decision 41). A folded tile's seconds are already that gap,
    // frozen at the fold, so they are not reduced again (decision 60).
    let computed = if folded { captured } else { (captured - logged).max(0.0) };
    let computed = (computed * 100.0).round() / 100.0;
    let override_hours = t.and_then(|t| t.hours_override);
    // Its own lines and those of the tiles folded into it, as the polish
    // reads them (decision 58).
    let mut lines: Vec<store::Line> =
        t.into_iter().chain(st.folded_into(item_id, date)).flat_map(|t| t.lines.iter().cloned()).collect();
    lines.sort_by_key(|l| l.ms);
    json!({
        "id": id,
        "date": date,
        "seconds": seconds,
        "hours": override_hours.or(t.and_then(|t| t.sent_hours)).unwrap_or(computed),
        "computed_hours": computed,
        "captured_hours": (captured * 100.0).round() / 100.0,
        "logged_hours": (logged * 100.0).round() / 100.0,
        "lines": lines_json(&lines),
        "note": t.and_then(|t| t.note.clone()),
        "harvest": st.tile_target(item_id, date).0,
        "harvest_from_desk": st.tile_target(item_id, date).1,
        "status": t.map(|t| t.status.clone()).unwrap_or_default(),
        "covered": override_hours.is_none() && logged > 0.0 && computed == 0.0,
        "folded_into": t.and_then(|t| t.folded_into.clone()),
    })
}

/// A tile with nothing left to send: Harvest already holds at least the
/// time it captured, and nobody set its hours by hand. It closes itself
/// (decision 57) and opens again if the entries covering it go.
fn covered(st: &State, item_id: &str, date: &str) -> bool {
    tile_json(st, item_id, date)["covered"] == true
}

/// The tiles still waiting for a ✓ or ✕: open, and not covered.
fn open_tile_keys(st: &State) -> Vec<(String, String)> {
    st.tile_keys()
        .into_iter()
        .filter(|(i, d)| st.tiles.get(&format!("{i}@{d}")).is_none_or(|t| t.status == TileStatus::Open))
        .filter(|(i, d)| !covered(st, i, d))
        .collect()
}

/// A session's state — `busy`, `waiting`, `idle` or `ended` — and what it
/// waits for: the hooks, which are instant, joined with `claude agents`,
/// read every 30 seconds — whichever saw the session last.
pub fn session_state(cache: &Cache, s: &store::Session) -> (&'static str, Option<String>) {
    let agent = cache.agents.get(&s.id).filter(|_| cache.agents_ms >= s.last_ms);
    let reported = agent.and_then(|a| match a["status"].as_str() {
        Some("busy") => Some("busy"),
        Some("waiting") => Some("waiting"),
        Some("idle") => Some("idle"),
        _ => None,
    });
    match reported {
        Some(st) => {
            let waiting = (st == "waiting")
                .then(|| agent.and_then(|a| a["waitingFor"].as_str()).map(str::to_string).or_else(|| s.waiting.clone()))
                .flatten();
            (st, waiting)
        }
        None if s.ended => ("ended", None),
        None if s.waiting.is_some() => ("waiting", s.waiting.clone()),
        None if s.busy => ("busy", None),
        None => ("idle", None),
    }
}

/// A card keeps this many of its ended panel chats.
const ENDED_CHATS_KEPT: usize = 5;

/// A card's live sessions (`embarch-desk-sessions-proposal.md` 6–8): the
/// state `claude agents --json` reports, the last reply and what it waits
/// for from the hooks, an Answer link on claude.ai (Remote Control) and a
/// Jump into VS Code. Its ended `embarch-chat` sessions stay too, the
/// newest [`ENDED_CHATS_KEPT`] within the card window, so Open chat can
/// resume one (chat-desk proposal 32).
fn session_rows(cache: &Cache, sessions: &[&store::Session], now: i64, window_days: i64) -> Vec<Value> {
    let live = |s: &store::Session| cache.agents.contains_key(&s.id) || (!s.ended && now - s.last_ms < 30 * 60_000);
    let mut ended: Vec<&store::Session> = sessions
        .iter()
        .copied()
        .filter(|s| !live(s) && s.ended && s.client.as_deref() == Some("embarch-chat") && now - s.last_ms < window_days * 86_400_000)
        .collect();
    ended.sort_by_key(|s| std::cmp::Reverse(s.last_ms));
    ended.truncate(ENDED_CHATS_KEPT);
    let mut rows: Vec<(i64, Value)> = sessions
        .iter()
        .copied()
        .filter(|s| live(s))
        .chain(ended)
        .map(|s| {
            let a = cache.agents.get(&s.id);
            let status = a.and_then(|a| a["status"].as_str()).unwrap_or(if s.ended { "ended" } else { "unknown" });
            let name = session_name(cache, s);
            let (state, waiting_for) = session_state(cache, s);
            (
                s.last_ms,
                json!({
                    "session_id": s.id,
                    "name": name,
                    "status": status,
                    "state": state,
                    "waiting_for": waiting_for,
                    "last_ms": s.last_ms,
                    "client": s.client,
                    "last_reply": s.last_reply,
                    "last_reply_ms": s.last_reply_ms,
                    "cwd": s.cwd,
                    "answer_url": format!("https://claude.ai/code/{}", s.id),
                    "folder_uri": (!s.cwd.is_empty()).then(|| uri::folder_uri(&s.cwd, cache.wsl_distro.as_deref())),
                    "session_uri": format!("vscode://anthropic.claude-code/open?session={}", uri::encode(&s.id)),
                }),
            )
        })
        .collect();
    rows.sort_by_key(|(ms, _)| std::cmp::Reverse(*ms));
    rows.into_iter().map(|(_, v)| v).collect()
}

fn session_name(cache: &Cache, s: &store::Session) -> String {
    cache.agents.get(&s.id).and_then(|a| a["name"].as_str()).map(str::to_string).unwrap_or_else(|| {
        std::path::Path::new(&s.cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "session".into())
    })
}

/// Every live session the desk tracks, on a card or not, for the panel's
/// thread list (chat proposal decisions 9, 20, 32): sessions under an
/// ignored path — the fleet — stay out; one the user untracked shows as such.
fn view_sessions(st: &State, cache: &Cache, now: i64) -> Vec<Value> {
    let mut rows: Vec<&store::Session> = st
        .sessions
        .values()
        .filter(|s| !State::is_timer_session(&s.id))
        .filter(|s| !s.ignored || !store::is_ignored(&s.cwd, &st.ignore))
        .filter(|s| cache.agents.contains_key(&s.id) || now - s.last_ms < 30 * 60_000)
        .collect();
    rows.sort_by_key(|s| std::cmp::Reverse(s.last_ms));
    rows.into_iter()
        .map(|s| {
            let (state, waiting_for) = session_state(cache, s);
            json!({
                "session_id": s.id,
                "name": session_name(cache, s),
                "item_id": s.item_id,
                "desk_id": s.desk_id,
                "untracked": s.ignored,
                "state": state,
                "waiting_for": waiting_for,
                "last_ms": s.last_ms,
                "cwd": s.cwd,
                "client": s.client,
            })
        })
        .collect()
}

/// The Desk tab's whole view. `desk` scopes the cards to that desk instead
/// of the open one, for a window on another desk (chat proposal 9).
pub fn view_json(st: &State, cache: &Cache, now: i64, window_days: i64, desk: Option<&str>, days: &dyn Days) -> Value {
    let today = days.date(now);
    let open = open_tile_keys(st);
    let scope = desk.map(str::to_string).or_else(|| st.open_desk.clone());
    let in_open = |i: &store::WorkItem| scope.is_none() || i.desk_id == scope;
    let mut items: Vec<&store::WorkItem> =
        st.items.values().filter(|i| visible(st, i, now, window_days, &open) && in_open(i)).collect();
    // The hand (decision 57): every tile waiting for a ✓ or ✕, on every
    // desk, so nothing waits unseen behind a switch — this desk's first,
    // then oldest first.
    let mut hand: Vec<(bool, Value)> = open
        .iter()
        .filter_map(|(i, d)| {
            let item = st.items.get(i)?;
            (!item.archived && (st.tile_seconds(i, d) > 0 || st.tiles.contains_key(&format!("{i}@{d}")))).then(|| {
                let mut t = tile_json(st, i, d);
                t["item_id"] = json!(item.id);
                t["key"] = json!(item.issue.as_ref().map(|r| r.key.clone()));
                t["item_title"] = json!(item.title);
                t["desk_id"] = json!(item.desk_id);
                t["desk"] = json!(item.desk_id.as_ref().and_then(|k| st.desks.get(k)).map(|k| k.name.clone()));
                (in_open(item), t)
            })
        })
        .collect();
    hand.sort_by(|a, b| (!a.0, a.1["date"].as_str()).cmp(&(!b.0, b.1["date"].as_str())));
    let hand: Vec<Value> = hand.into_iter().map(|(_, t)| t).collect();
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
                "repo_path": item.repo_path.clone().or_else(|| {
                    item.desk_id.as_ref().and_then(|k| st.desks.get(k)).and_then(|d| d.firmware_repos.first().cloned())
                }),
                "prs": item.prs,
                "pinned": item.pinned,
                "last_activity_ms": item.last_activity_ms,
                "sessions": { "live": live, "today": today_sessions, "total": sessions.len() },
                "logged_today": (st.logged_hours(&item.id, &today) * 100.0).round() / 100.0,
                "live_sessions": session_rows(cache, &sessions, now, window_days),
                "notes_ms": st.notes_ms(&item.id),
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
        "sessions": view_sessions(st, cache, now),
        "settings": st.settings,
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
        "hand": hand,
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

/// A running timer's span moves up to `now`; with none running, the state
/// stays clean.
fn tick_timer(app: &App, now: i64) {
    if app.db.read(|st| st.timer.is_some()) {
        app.db.write(|st| st.tick_timer(now, &LocalDays));
    }
}

#[derive(Deserialize, Default)]
struct ViewQuery {
    #[serde(default)]
    desk: Option<String>,
}

/// The desk a `?desk=` names (by id or name; 404 when there is none), or
/// none when it names none.
fn named_desk(app: &App, q: &ViewQuery) -> Result<Option<String>, ApiError> {
    match clean(&q.desk) {
        Some(d) => Ok(Some(app.db.read(|st| st.desk_by_name_or_id(&d).map(|k| k.id.clone())).ok_or_else(|| missing(format!("no desk {d}")))?)),
        None => Ok(None),
    }
}

async fn view(Ax(app): Ax<Shared>, Query(q): Query<ViewQuery>) -> ApiResult {
    let now = now_ms();
    let desk = named_desk(&app, &q)?;
    // A read changes nothing, so a polling window never rewrites the state
    // file. A running timer's tile catches up in the sync loop, within 30
    // seconds; the view's timer counts from its start.
    let (mut v, viewed) = {
        let cache = app.cache.lock().expect("cache");
        app.db.read(|st| (view_json(st, &cache, now, app.window_days, desk.as_deref(), &LocalDays), desk.clone().or(st.open_desk.clone())))
    };
    // When the viewed desk's reference index last changed: a client
    // re-reads `/refs` only when it moves (decision 56).
    v["refs_ms"] = json!(viewed.and_then(|d| app.refs.synced_ms(&d)));
    Ok(Json(v))
}

/// The reference index of the named desk, else the open one (decision 56):
/// every issue in its Linear scope and every PR in its repos, from memory —
/// the sync loop refreshes it, so a request never waits on Linear or GitHub.
/// With no desk open and none named, an empty index naming no desk.
async fn refs_route(Ax(app): Ax<Shared>, Query(q): Query<ViewQuery>) -> ApiResult {
    let desk = named_desk(&app, &q)?.or_else(|| app.db.read(|st| st.open_desk.clone()));
    Ok(Json(app.refs.json(desk.as_deref())))
}

// ---------------------------------------------------------------- hooks

async fn hook(Ax(app): Ax<Shared>, Json(ev): Json<HookEvent>) -> ApiResult {
    let now = now_ms();
    // A memory file's text is read here, after the write, so `events` stays
    // pure.
    let memory = crate::memory::claude_dir().and_then(|d| {
        let p = events::memory_write(&ev, &d)?;
        crate::memory::read_text(std::path::Path::new(&p), &d).map(|t| (p, t))
    });
    let ctx = app.db.write(|st| {
        let ctx = events::apply(st, &ev, now, &LocalDays, &RealGit, app.window_days);
        if let Some((path, text)) = &memory {
            events::capture_memory(st, &ev.session_id, path, text, now, &LocalDays);
        }
        ctx
    });
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
                // Its own repo first (decision 8), else its desk's.
                let desk_repo = i
                    .desk_id
                    .as_ref()
                    .and_then(|k| st.desks.get(k))
                    .and_then(|d| d.firmware_repos.first().cloned());
                let what = match &i.issue {
                    Some(r) => format!("{}: {} ({})", r.key, r.title, r.url),
                    None => i.title.clone(),
                };
                let prs: Vec<String> = i.prs.iter().filter(|p| p.state == "open").map(|p| p.url.clone()).collect();
                let prs = if prs.is_empty() { String::new() } else { format!(" Open PR: {}.", prs.join(", ")) };
                (i.repo_path.clone().or(desk_repo), format!("Work on {what}.{prs} (EmbArch desk work item {}.)", i.id))
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
                "client": s.client,
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
    /// `embarch-chat` when the panel runs the session (chat proposal 10).
    #[serde(default)]
    client: Option<String>,
}

async fn assign_session(Ax(app): Ax<Shared>, Path(sid): Path<String>, Json(b): Json<Assign>) -> ApiResult {
    let now = now_ms();
    let client = clean(&b.client).map(|c| c.chars().take(40).collect::<String>());
    app.db.write(|st| {
        let s = st.sessions.entry(sid.clone()).or_insert_with(|| store::Session::new(&sid, "", now));
        if client.is_some() {
            s.client = client;
        }
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
    /// `done` (the default), `finding`, `decision`, `handoff` or `todo`.
    #[serde(default)]
    kind: Option<String>,
}

impl LineBody {
    /// The note it makes, its kind checked.
    fn note(&self, ms: i64, source: &str) -> Result<store::Line, ApiError> {
        let mut note = store::Line::new(ms, self.text.trim(), source);
        if let Some(k) = clean(&self.kind) {
            let k = k.to_ascii_lowercase();
            if !store::NOTE_KINDS.contains(&k.as_str()) {
                return Err(bad(format!("unknown kind {k:?}; one of {}", store::NOTE_KINDS.join(", "))));
            }
            note.kind = k;
        }
        Ok(note)
    }
}

async fn session_line(Ax(app): Ax<Shared>, Path(sid): Path<String>, Json(b): Json<LineBody>) -> ApiResult {
    let now = now_ms();
    let today = LocalDays.date(now);
    let mut note = b.note(now, "agent")?;
    note.session_id = Some(sid.clone());
    let landed = app.db.write(|st| {
        let sess = st.sessions.get_mut(&sid)?;
        match sess.item_id.clone() {
            Some(item) => {
                let id = st.add_note(&item, &today, note);
                Some(json!({ "tile": format!("{item}@{today}"), "note_id": id }))
            }
            None => {
                sess.held_lines.push(note);
                Some(json!({ "held": true, "note": "the session has no work item yet; the line lands when it gets one" }))
            }
        }
    });
    landed.map(Json).ok_or_else(|| missing("the desk has not seen this session"))
}

// ---------------------------------------------------------------- tiles

fn split_tile_id(id: &str) -> Result<(String, String), ApiError> {
    let (i, d) = id.rsplit_once('@').ok_or_else(|| bad("a tile id is <item>@<date>"))?;
    if !store::is_date(d) {
        return Err(bad("a tile id is <item>@<YYYY-MM-DD>"));
    }
    Ok((i.to_string(), d.to_string()))
}

fn require_open(st: &State, id: &str) -> Result<(), ApiError> {
    match st.tiles.get(id).map(|t| t.status.clone()) {
        Some(TileStatus::Approved) => Err(bad("this tile is already in Harvest")),
        Some(TileStatus::Discarded) => Err(bad("this tile was discarded")),
        Some(TileStatus::Folded) => Err(bad("this tile was folded into a later one")),
        _ => Ok(()),
    }
}

async fn tile_line(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<LineBody>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    let note = b.note(now_ms(), "user")?;
    app.db.write(|st| {
        require_open(st, &id)?;
        st.add_note(&item, &date, note);
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(app.db.read(|st| tile_json(st, &item, &date))))
}

#[derive(Deserialize)]
struct TilePatch {
    #[serde(default)]
    note: Option<String>,
    /// Absent leaves the override alone; a number sets it; `null` clears it
    /// so the tile goes back to computed hours (decision 61).
    #[serde(default, deserialize_with = "some_or_null")]
    hours: Option<Option<f64>>,
    #[serde(default)]
    harvest: Option<HarvestTarget>,
}

fn some_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<f64>>, D::Error> {
    Option::<f64>::deserialize(d).map(Some)
}

async fn patch_tile(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<TilePatch>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    if b.hours.flatten().is_some_and(|h| !(0.0..=24.0).contains(&h)) {
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
            t.hours_override = h;
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
    let r = harvest::recent_entries(&conns.harvest, 45).await.map(|e| e.entries).unwrap_or_default();
    app.cache.lock().expect("cache").harvest_recent = Some((now, r.clone()));
    r
}

async fn harvest_targets(Ax(app): Ax<Shared>) -> ApiResult {
    Ok(Json(json!({ "targets": cached_targets(&app).await? })))
}

pub fn polish_prompt(item_label: &str, date: &str, hours: f64, lines: &[store::Line], targets: &[harvest::Target], recent: &[Value]) -> String {
    let mut p = format!(
        "Work item: {item_label}\nDate: {date}, about {hours:.2} hours.\n\nWhat was done, from the day's notes \
         (commits, milestones, findings, decisions, handoffs, to-dos, and the memory files the sessions kept):\n"
    );
    if lines.is_empty() {
        p.push_str("- (no notes were recorded; write from the work item alone)\n");
    }
    // The newest 60: a busy day's end — its handoff, its to-dos — is what
    // must not fall off.
    let skip = lines.len().saturating_sub(60);
    if skip > 0 {
        p.push_str(&format!("- ({skip} earlier notes left out)\n"));
    }
    for l in lines.iter().skip(skip) {
        match l.kind.as_str() {
            "done" => p.push_str(&format!("- [{}] {}\n", l.source, l.text)),
            "memory" => {
                let name = l.source_path.as_deref().and_then(|p| p.rsplit('/').next()).unwrap_or("memory");
                let flat = l.text.split_whitespace().collect::<Vec<_>>().join(" ");
                p.push_str(&format!("- [memory file {name}, as it stood] {}\n", crate::brief::cut(&flat, 800)));
            }
            k => p.push_str(&format!("- [{k}] {}\n", l.text)),
        }
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

/// Opening a tile turns that day's notes into one Harvest note and guesses
/// where the time goes — one `claude -p` call per tile (decision 13; what
/// goes to Harvest is the summary of the day's notes, chat-desk proposal 43).
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
        // That day's notes of every kind (proposal 43).
        let lines = st.day_notes(&item, &date, &LocalDays);
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

/// The Harvest note when nobody wrote one: the lines the dialog shows, the
/// tile's own and those of the tiles folded into it, oldest first.
fn approve_fallback_note(st: &State, item_id: &str, date: &str) -> String {
    let t = st.tiles.get(&format!("{item_id}@{date}"));
    let mut lines: Vec<&store::Line> = t.into_iter().chain(st.folded_into(item_id, date)).flat_map(|t| t.lines.iter()).collect();
    lines.sort_by_key(|l| l.ms);
    lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("; ")
}

/// A typed override sends what the person typed, so a gap that moved under
/// it is the one thing ✓ refuses over, once (decision 62); with no override
/// ✓ sends the fresh gap.
fn gap_moved_under_override(before: f64, after: f64, typed: Option<f64>) -> bool {
    typed.is_some() && (before - after).abs() > 0.005
}

/// How long a past day's Harvest entries are trusted before the sync loop
/// reads that day again (decision 62) [chosen].
const PAST_DAY_REFRESH_MS: i64 = 10 * 60_000;

/// The earlier days with a waiting tile whose Harvest read is older than
/// `PAST_DAY_REFRESH_MS`: one per distinct day, never one per tile, so a
/// pass makes at most one read per such day.
fn past_days_due(st: &State, fetched: &HashMap<String, i64>, today: &str, now: i64) -> Vec<String> {
    let mut days: Vec<String> = st
        .tile_keys()
        .into_iter()
        .filter(|(i, d)| d.as_str() < today && st.tiles.get(&format!("{i}@{d}")).is_none_or(|t| t.status == TileStatus::Open))
        .map(|(_, d)| d)
        .filter(|d| now - fetched.get(d).copied().unwrap_or(0) > PAST_DAY_REFRESH_MS)
        .collect();
    days.sort();
    days.dedup();
    days
}

/// ✓: the tile becomes a Harvest time entry. The human's alone — no MCP
/// tool reaches this route.
async fn approve_tile(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    // Re-read the tile's day before computing what to send (decision 62): an
    // entry typed into Harvest since the day was last read would otherwise
    // be sent twice. A failed read falls back to what is stored.
    let before = app.db.read(|st| {
        require_open(st, &id)?;
        Ok::<_, ApiError>((tile_json(st, &item, &date)["computed_hours"].as_f64().unwrap_or(0.0), st.tiles.get(&id).and_then(|t| t.hours_override)))
    })?;
    if let Err(e) = refresh_logged(&app, &date).await {
        tracing::warn!("re-reading Harvest for {date} before the entry: {e:#}");
    }
    let (target, note, hours, seconds) = app.db.read(|st| {
        require_open(st, &id)?;
        let t = st.tiles.get(&id);
        let target = st.tile_target(&item, &date).0.ok_or_else(|| bad("pick a Harvest project and task first, or Polish to have Claude guess one"))?;
        let note = t
            .and_then(|t| t.note.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| approve_fallback_note(st, &item, &date));
        let j = tile_json(st, &item, &date);
        Ok::<_, ApiError>((target, note, j["hours"].as_f64().unwrap_or(0.0), j["seconds"].as_i64().unwrap_or(0)))
    })?;
    let after = app.db.read(|st| tile_json(st, &item, &date)["computed_hours"].as_f64().unwrap_or(0.0));
    if gap_moved_under_override(before.0, after, before.1) {
        return Err(bad(format!(
            "Harvest changed for {date} since this tile was read: the gap is now {after:.2} h, was {:.2} h, and you typed {:.2} h; check the hours and press send again",
            before.0,
            before.1.unwrap_or(0.0)
        )));
    }
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
        t.sent_hours = Some(hours);
    });
    if let Err(e) = refresh_logged(&app, &date).await {
        tracing::warn!("re-reading Harvest after the entry: {e:#}");
    }
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

#[derive(Deserialize)]
struct FoldBody {
    /// The date of the item's tile that takes this one's time and lines.
    into: String,
}

/// Fold a tile into a later one of the same item (decision 58): one Harvest
/// entry, dated the later day, carries both.
async fn fold_tile(Ax(app): Ax<Shared>, Path(id): Path<String>, Json(b): Json<FoldBody>) -> ApiResult {
    let (item, date) = split_tile_id(&id)?;
    let now = now_ms();
    app.db.write(|st| st.fold_tile(&item, &date, &b.into, now)).map_err(bad)?;
    Ok(Json(app.db.read(|st| tile_json(st, &item, &b.into))))
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
    let (issues, truncated) = linear::my_issues(&linear_accounts()?, scope.as_ref()).await?;
    Ok(Json(json!({
        "scope": scope.as_ref().map(|b| b.project_name.clone()),
        "issues": issues,
        "truncated": truncated,
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
    let read = harvest::entries_between(&conns.harvest, &from, &to).await?;
    let (entries, truncated) = (read.entries, read.truncated);
    let total: f64 = entries.iter().filter_map(|e| e["hours"].as_f64()).sum();
    let mut out = json!({ "from": from, "to": to, "total_hours": (total * 100.0).round() / 100.0, "entries": entries, "truncated": truncated });
    if truncated {
        out["note"] = json!(format!(
            "Cut at {} entries per Harvest account: total_hours is a lower bound. Ask for a shorter range.",
            harvest::ENTRIES_CAP
        ));
    }
    Ok(Json(out))
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
    // Its scope may have moved: its reference index refreshes on the next tick.
    app.refs.forget_checked(Some(&id));
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
        let lines = lines_json(tile.map(|t| t.lines.as_slice()).unwrap_or_default());
        let entries: Vec<&store::LoggedEntry> = st
            .harvest_logged
            .get(date)
            .into_iter()
            .flatten()
            .filter(|e| st.item_for_entry(e).as_deref() == Some(item.id.as_str()))
            .collect();
        if seconds == 0 && lines.is_empty() && bench.is_empty() && sessions.is_empty() && entries.is_empty() {
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
                "captured_hours": t["captured_hours"],
                "logged_hours": entries.iter().map(|e| e.hours).sum::<f64>(),
                "harvest_entries": entries,
                "status": t["status"],
                "folded_into": t["folded_into"],
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
    // Entries no card claims — a call, an internal task, a key with no card
    // yet — are rows of their own, under the desk billing their project.
    for e in st.harvest_logged.get(date).into_iter().flatten().filter(|e| st.item_for_entry(e).is_none()) {
        let title = e.notes.lines().next().filter(|l| !l.trim().is_empty()).map(str::to_string)
            .unwrap_or_else(|| format!("{} / {}", e.project, e.task));
        rows.push((
            st.desk_for_entry(e),
            json!({
                "id": null, "title": title, "issue": null, "prs": [], "hours": 0.0, "captured_hours": 0.0,
                "logged_hours": e.hours, "harvest_entries": [e], "status": "harvest_only", "harvest": null,
                "lines": [], "note": null, "sessions": 0, "live": 0, "bench": 0, "bench_failed": 0,
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
    let unsent: f64 = groups
        .iter()
        .flat_map(|g| g.3.iter())
        .filter(|r| r["status"] == "open")
        .filter_map(|r| r["hours"].as_f64())
        .sum();
    let logged: f64 = st.harvest_logged.get(date).into_iter().flatten().map(|e| e.hours).sum();
    let total = unsent + logged;
    json!({
        "date": date,
        "total_hours": (total * 100.0).round() / 100.0,
        "logged_hours": (logged * 100.0).round() / 100.0,
        "unsent_hours": (unsent * 100.0).round() / 100.0,
        "groups": groups.into_iter().map(|(_, name, desk, items)| json!({ "desk": desk, "name": name, "items": items })).collect::<Vec<_>>(),
    })
}

async fn day(Ax(app): Ax<Shared>, Path(date): Path<String>) -> ApiResult {
    if chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
        return Err(bad(format!("{date} is not a YYYY-MM-DD date")));
    }
    let now = now_ms();
    let fetched = app.cache.lock().expect("cache").logged_fetched.get(&date).copied().unwrap_or(0);
    if now - fetched > 2 * 60_000 {
        if let Err(e) = refresh_logged(&app, &date).await {
            tracing::warn!("reading Harvest for {date}: {e:#}");
        }
    }
    Ok(Json(app.db.read(|st| day_json(st, &date, now))))
}

// ---------------------------------------------------------------- picking up an issue
//
// embarch-desk-sessions-proposal.md: the open desk's issues with no card
// yet, and Start work — the issue to In Progress, a worktree beside the main
// clone on Linear's branch, auto mode, the launch claim, and the VS Code
// links with the read-and-summarize prompt pre-filled. The owner presses
// Enter: nothing documented submits a VS Code chat from outside.

async fn pickup(Ax(app): Ax<Shared>) -> ApiResult {
    let scope = open_linear(&app);
    let (issues, _) = linear::my_issues(&linear_accounts()?, scope.as_ref()).await?;
    let carded: std::collections::HashSet<String> = app.db.read(|st| {
        st.items
            .values()
            .filter(|i| !i.archived)
            .filter_map(|i| i.issue.as_ref().map(|r| r.key.to_ascii_uppercase()))
            .collect()
    });
    let open: Vec<Value> = issues
        .into_iter()
        .filter(|i| i["key"].as_str().is_some_and(|k| !carded.contains(&k.to_ascii_uppercase())))
        .collect();
    Ok(Json(json!({ "scope": scope.map(|b| if b.project_name.is_empty() { format!("team {}", b.team_key) } else { b.project_name }), "issues": open })))
}

pub fn start_prompt(issue: &store::IssueRef, item_id: &str, branch: &str) -> String {
    format!(
        "Read {} — {} ({}) with the desk's linear_get_issue tool: the description and every comment, and \
         its PR if one is linked. Summarize what is asked, what is already done and what is still open, \
         then wait for my first instruction. (EmbArch desk work item {item_id}; this worktree is on {branch}.)",
        issue.key, issue.title, issue.url
    )
}

/// The clone a worktree comes from: the card's own repo when it is a main
/// clone (a worktree's `.git` is a file, a clone's a directory), else the
/// desk's first firmware repo.
fn main_clone(st: &State, item: &store::WorkItem) -> Option<String> {
    item.repo_path
        .clone()
        .filter(|p| std::path::Path::new(p).join(".git").is_dir())
        .or_else(|| item.desk_id.as_ref().and_then(|k| st.desks.get(k)).and_then(|d| d.firmware_repos.first().cloned()))
}

async fn start_work(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let now = now_ms();
    let (issue, main) = app
        .db
        .read(|st| st.items.get(&id).map(|i| (i.issue.clone(), main_clone(st, i))))
        .ok_or_else(|| missing("no such work item"))?;
    let issue = issue.ok_or_else(|| bad("Start work needs a Linear issue on the card"))?;
    let main = main.ok_or_else(|| ApiError(StatusCode::CONFLICT, "no repo to start from: pick the card's repo, or give its desk a firmware repo".into()))?;
    let accounts = linear_accounts()?;
    // Fresh from Linear: its branch name and state may have changed.
    let issue = linear::find_issue(&accounts, &issue.key, now).await?.unwrap_or(issue);
    let branch = if issue.branch_name.is_empty() {
        issue.key.to_ascii_lowercase()
    } else {
        issue.branch_name.clone()
    };
    let mut warnings: Vec<String> = vec![];
    let mut moved_to = None;
    match linear::states(&accounts, &issue).await {
        Ok(states) => {
            if let Some(sid) = linear::start_state(&states, &issue.state_type) {
                match linear::set_state(&accounts, &issue, &sid, now).await {
                    Ok(updated) => {
                        moved_to = Some(updated.state_name.clone());
                        app.db.write(|st| st.items.get_mut(&id).map(|i| i.issue = Some(updated)));
                    }
                    Err(e) => warnings.push(format!("could not move the issue: {e:#}")),
                }
            }
        }
        Err(e) => warnings.push(format!("could not read the team's states: {e:#}")),
    }
    let key = issue.key.clone();
    let br = branch.clone();
    let main_path = std::path::PathBuf::from(&main);
    let prepared = tokio::task::spawn_blocking(move || {
        let p = crate::worktree::prepare(&main_path, &key, &br)?;
        crate::worktree::set_auto_mode(&p.path)?;
        Ok::<_, anyhow::Error>(p)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    warnings.extend(prepared.warnings.iter().cloned());
    let wt = prepared.path.display().to_string();
    let prompt = start_prompt(&issue, &id, &branch);
    let mailbox_id = app.db.write(|st| {
        if let Some(i) = st.items.get_mut(&id) {
            i.repo_path = Some(wt.clone());
            i.last_activity_ms = now;
        }
        st.pending_launches.retain(|p| p.item_id != id);
        st.pending_launches.push(PendingLaunch { item_id: id.clone(), repo: wt.clone(), expires_ms: now + store::LAUNCH_CLAIM_MS });
        // With the chat panel on, the worktree's window takes the start from
        // the mailbox and sends the prompt itself (chat-desk proposal 11, 33).
        st.settings.chat_panel.then(|| st.post_mail(start_entry(&id, &wt, &prompt), now))
    });
    let mut out = json!({
        "worktree": wt,
        "branch": prepared.branch,
        "how": prepared.how,
        "moved_to": moved_to,
        "warnings": warnings,
        "folder_uri": uri::folder_uri(&wt, app.wsl_distro.as_deref()),
    });
    match mailbox_id {
        Some(m) => out["mailbox_id"] = json!(m),
        None => out["claude_uri"] = json!(uri::claude_uri(&prompt)),
    }
    Ok(Json(out))
}

fn start_entry(item_id: &str, folder: &str, prompt: &str) -> MailboxEntry {
    MailboxEntry {
        id: String::new(),
        kind: "start".into(),
        item_id: Some(item_id.to_string()),
        session_id: None,
        folder: folder.to_string(),
        prompt: Some(prompt.to_string()),
        created_ms: 0,
        expires_ms: 0,
        taken_ms: None,
    }
}

// ---------------------------------------------------------------- the chat panel
//
// embarch-chat-desk-proposal.md: the mailbox the panel polls (33), the
// setting that sends Start work into it (11), the pull a chat reads (40) and
// the work item's notes (46).

#[derive(Deserialize)]
struct NewMail {
    kind: String,
    #[serde(default)]
    session_id: Option<String>,
    folder: String,
}

/// Leave a focus — bring up a session's thread — for the window whose folder
/// matches. It expires as a launch claim does. A start is left by Start
/// work alone, with the prompt the desk writes: a route taking a caller's
/// prompt would let any page that reaches the desk run one in auto mode.
async fn post_mailbox(Ax(app): Ax<Shared>, Json(b): Json<NewMail>) -> ApiResult {
    let folder = b.folder.trim();
    if !folder.starts_with('/') {
        return Err(bad("folder must be an absolute path"));
    }
    let session_id = clean(&b.session_id);
    match b.kind.as_str() {
        "focus" if session_id.is_none() => return Err(bad("a focus needs session_id")),
        "focus" => {}
        "start" => return Err(bad("a start is left by Start work (POST /items/{id}/start), not posted")),
        k => return Err(bad(format!("unknown kind {k:?}; focus"))),
    }
    let entry = MailboxEntry {
        id: String::new(),
        kind: "focus".into(),
        item_id: None,
        session_id,
        folder: folder.to_string(),
        prompt: None,
        created_ms: 0,
        expires_ms: 0,
        taken_ms: None,
    };
    let id = app.db.write(|st| st.post_mail(entry, now_ms()));
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct MailQuery {
    #[serde(default)]
    folder: Option<String>,
}

async fn get_mailbox(Ax(app): Ax<Shared>, Query(q): Query<MailQuery>) -> ApiResult {
    let folder = clean(&q.folder).filter(|f| f.starts_with('/')).ok_or_else(|| bad("give folder, an absolute path"))?;
    let now = now_ms();
    Ok(Json(app.db.read(|st| json!({ "entries": st.mail_for(&folder, now) }))))
}

/// The entry, once: a second take is a 409.
async fn take_mailbox(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    match app.db.write(|st| st.take_mail(&id, now_ms())) {
        Ok(m) => Ok(Json(json!(m))),
        Err(TakeError::Unknown) => Err(missing("no such mailbox entry")),
        Err(TakeError::Taken) => Err(ApiError(StatusCode::CONFLICT, "this entry was already taken".into())),
        Err(TakeError::Expired) => Err(ApiError(StatusCode::CONFLICT, "this entry expired".into())),
    }
}

async fn get_settings(Ax(app): Ax<Shared>) -> ApiResult {
    Ok(Json(json!(app.db.read(|st| st.settings.clone()))))
}

#[derive(Deserialize)]
struct SettingsPatch {
    #[serde(default)]
    chat_panel: Option<bool>,
}

async fn put_settings(Ax(app): Ax<Shared>, Json(b): Json<SettingsPatch>) -> ApiResult {
    let s = app.db.write(|st| {
        if let Some(on) = b.chat_panel {
            st.settings.chat_panel = on;
        }
        st.settings.clone()
    });
    Ok(Json(json!(s)))
}

/// The pull (proposal 40): everything about the item, newest first, cut at
/// 16,000 characters. The issue is read fresh from Linear; when it cannot
/// be, the brief says so and carries the rest.
async fn item_brief(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    let key = app
        .db
        .read(|st| st.items.get(&id).map(|i| i.issue.as_ref().map(|r| r.key.clone())))
        .ok_or_else(|| missing("no such work item"))?;
    let (issue, issue_error) = match key {
        None => (None, None),
        Some(k) => match linear_accounts() {
            Ok(accounts) => match linear::read_issue(&accounts, &k).await {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            },
            Err(e) => (None, Some(e.1)),
        },
    };
    let cache = app.cache.lock().expect("cache");
    let text = app
        .db
        .read(|st| brief::render(st, &cache, &id, issue.as_ref(), issue_error.as_deref(), &LocalDays))
        .ok_or_else(|| missing("no such work item"))?;
    Ok(Json(json!({ "item_id": id, "text": text, "chars": text.chars().count(), "max_chars": brief::BRIEF_MAX })))
}

/// Every note on the item, newest first (proposal 46).
async fn item_notes(Ax(app): Ax<Shared>, Path(id): Path<String>) -> ApiResult {
    app.db.read(|st| {
        if !st.items.contains_key(&id) {
            return Err(missing("no such work item"));
        }
        let latest: Vec<&str> = st.memory_latest(&id).iter().map(|l| l.id.as_str()).collect();
        let notes: Vec<Value> = st
            .item_notes(&id)
            .into_iter()
            .map(|l| brief::note_json(l, !l.is_memory() || latest.contains(&l.id.as_str()), &LocalDays))
            .collect();
        Ok(Json(json!({ "item_id": id, "notes": notes })))
    })
}

/// Attach the memory files written before capture existed to the items
/// whose issue key they hold — once, on the first start with notes
/// (proposal 47). It runs beside the server, never ahead of it: hooks wait
/// on nothing.
pub async fn import_memories(app: &App) {
    if app.db.read(|st| st.memory_imported) {
        return;
    }
    let Some(dir) = crate::memory::claude_dir() else { return };
    let ignore = app.db.read(|st| st.ignore.clone());
    let Ok(files) = tokio::task::spawn_blocking(move || crate::memory::scan(&dir, &ignore)).await else { return };
    let added = app.db.write(|st| st.import_memories(&files, now_ms()));
    tracing::info!("attached {added} memory notes from {} memory files", files.len());
}

// ---------------------------------------------------------------- what Harvest already holds
//
// The day list merges the entries Harvest already has — logged by hand, by
// the Harvest app, or by a tile's ✓ — into the work items whose issue their
// note names (decision 41). An entry naming an issue with no card makes the
// card. The user can edit or delete an entry from the list.

fn noon_ms(date: &str) -> Option<i64> {
    use chrono::TimeZone;
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    chrono::Local.from_local_datetime(&d.and_hms_opt(12, 0, 0)?).earliest().map(|t| t.timestamp_millis())
}

pub async fn refresh_logged(app: &App, date: &str) -> anyhow::Result<()> {
    let conns = Connectors::load()?;
    if conns.harvest.is_empty() {
        return Ok(());
    }
    let raw = harvest::entries_between(&conns.harvest, date, date).await?.entries;
    let entries: Vec<store::LoggedEntry> = raw
        .iter()
        .map(|e| {
            let notes = e["notes"].as_str().unwrap_or_default().to_string();
            store::LoggedEntry {
                id: e["id"].as_u64().unwrap_or(0),
                account_id: e["account_id"].as_u64().unwrap_or(0),
                project_id: e["project_id"].as_u64().unwrap_or(0),
                task_id: e["task_id"].as_u64().unwrap_or(0),
                client: e["client"].as_str().unwrap_or_default().to_string(),
                project: e["project"].as_str().unwrap_or_default().to_string(),
                task: e["task"].as_str().unwrap_or_default().to_string(),
                hours: e["hours"].as_f64().unwrap_or(0.0),
                running: e["is_running"].as_bool().unwrap_or(false),
                issue_key: store::issue_key_in(&notes),
                notes,
            }
        })
        .collect();
    let missing: Vec<String> = app.db.write(|st| {
        st.harvest_logged.insert(date.to_string(), entries.clone());
        let mut keys: Vec<String> = entries
            .iter()
            .filter_map(|e| e.issue_key.clone())
            .filter(|k| st.item_by_key(k).is_none())
            .collect();
        keys.sort();
        keys.dedup();
        keys
    });
    // An entry naming an issue makes that issue's card, dated by the entry
    // so an old day read back does not look like today's work.
    if !missing.is_empty() && !conns.linear.is_empty() {
        let now = now_ms();
        let at = noon_ms(date).unwrap_or(now).min(now);
        for key in missing {
            if let Ok(Some(issue)) = linear::find_issue(&conns.linear, &key, now).await {
                app.db.write(|st| {
                    if st.item_by_key(&key).is_none() {
                        let title = issue.title.clone();
                        let id = st.create_item(&title, Some(issue), at);
                        if let Some(i) = st.items.get_mut(&id) {
                            i.created_ms = at;
                            i.last_activity_ms = at;
                        }
                    }
                });
            }
        }
    }
    app.cache.lock().expect("cache").logged_fetched.insert(date.to_string(), now_ms());
    Ok(())
}

fn entry_date(app: &App, id: u64) -> Option<String> {
    app.db.read(|st| st.harvest_logged.iter().find(|(_, es)| es.iter().any(|e| e.id == id)).map(|(d, _)| d.clone()))
}

#[derive(Deserialize)]
struct EntryEdit {
    #[serde(default)]
    hours: Option<f64>,
    #[serde(default)]
    notes: Option<String>,
}

/// The user's edit of an entry already in Harvest, sent as they left it.
async fn edit_entry(Ax(app): Ax<Shared>, Path((account, id)): Path<(u64, u64)>, Json(b): Json<EntryEdit>) -> ApiResult {
    if b.hours.is_some_and(|h| !(0.0..=24.0).contains(&h)) {
        return Err(bad("hours must be between 0 and 24"));
    }
    let conns = Connectors::load()?;
    harvest::update_entry(&conns.harvest, account, id, b.hours, b.notes.as_deref()).await?;
    if let Some(date) = entry_date(&app, id) {
        refresh_logged(&app, &date).await?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// Delete an entry from Harvest. A tile whose ✓ made it opens again, so its
/// time is not lost with the entry.
async fn remove_entry(Ax(app): Ax<Shared>, Path((account, id)): Path<(u64, u64)>) -> ApiResult {
    let conns = Connectors::load()?;
    harvest::delete_entry(&conns.harvest, account, id).await?;
    app.db.write(|st| {
        st.reopen_tiles_of_entry(id);
    });
    if let Some(date) = entry_date(&app, id) {
        refresh_logged(&app, &date).await?;
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- sync

async fn sync_now(Ax(app): Ax<Shared>) -> ApiResult {
    {
        let mut c = app.cache.lock().expect("cache");
        c.last_status_ms = 0;
        c.last_slack_ms = 0;
        c.pr_checked.clear();
    }
    // Every desk's reference index is due, and its task is woken rather than
    // waited on: a refresh waiting out its timeouts is not this request's.
    app.refs.forget_checked(None);
    app.refs.wake();
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

    if let Some((asked_ms, agents)) = live_agents("claude").await {
        let mut c = app.cache.lock().expect("cache");
        c.agents = agents;
        c.agents_ms = asked_ms;
    }
    let today = LocalDays.date(now);
    let fetched = app.cache.lock().expect("cache").logged_fetched.get(&today).copied().unwrap_or(0);
    if force_issues || now - fetched > 2 * 60_000 {
        if let Err(e) = refresh_logged(app, &today).await {
            tracing::warn!("reading Harvest for {today}: {e:#}");
        }
    }
    let due = {
        let fetched = app.cache.lock().expect("cache").logged_fetched.clone();
        app.db.read(|st| past_days_due(st, &fetched, &today, now))
    };
    for day in due {
        if let Err(e) = refresh_logged(app, &day).await {
            tracing::warn!("reading Harvest for {day}: {e:#}");
            // Back off a failed day for the interval rather than retry every pass.
            app.cache.lock().expect("cache").logged_fetched.insert(day, now);
        }
    }
}

/// Every desk's reference index whose five minutes are up (decision 56),
/// one desk at a time, each claimed before its fetch; a deleted desk's
/// index goes.
async fn refresh_refs<F, Fut>(app: &App, mut fetch: F)
where
    F: FnMut(refs::Scope) -> Fut,
    Fut: std::future::Future<Output = refs::Fetched>,
{
    let desks: Vec<(String, refs::Scope)> = app.db.read(|st| st.desks.values().map(|d| (d.id.clone(), refs::Scope::of(d))).collect());
    app.refs.retain(&desks.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>());
    for (id, scope) in desks {
        if !app.refs.claim(&id, now_ms()) {
            continue;
        }
        let started = std::time::Instant::now();
        let got = fetch(scope.clone()).await;
        let changed = app.refs.apply(&id, &scope, got, now_ms());
        tracing::debug!("reference index {id}: refreshed in {} ms, changed: {changed}", started.elapsed().as_millis());
    }
}

/// The reference index's own task (decision 56): a pass every 30 seconds,
/// and at once when `POST /sync` wakes it, over the desks whose five
/// minutes are up. It runs beside the sync loop, never inside it: on a
/// network that drops packets every request waits out its timeout, and that
/// must hold up neither timer catch-up, claim and mail expiry, the Slack
/// feed nor `POST /sync`. Only this task refreshes, so passes never overlap.
pub async fn refs_loop(app: Shared) {
    refs_loop_with(app, std::time::Duration::from_secs(30), |app| async move {
        let Ok(conns) = Connectors::load() else { return };
        let gh = conns.github_accounts();
        let (linear, gh) = (&conns.linear, &gh);
        refresh_refs(&app, |scope| async move { refs::fetch(linear, gh, &scope).await }).await;
    })
    .await
}

async fn refs_loop_with<P, Fut>(app: Shared, every: std::time::Duration, mut pass: P)
where
    P: FnMut(Shared) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = app.refs.woken() => {}
        }
        pass(app.clone()).await;
    }
}

/// The sync loop's other half, every 30 seconds whatever the connectors
/// do: a running timer moves up to `now`, and stale tiles, sessions, claims
/// and mail leave the live state. A pass that changes nothing leaves
/// state.json alone.
pub fn housekeep(app: &App, now: i64) {
    tick_timer(app, now);
    let archived = app.db.update(|st| st.prune(now));
    if let Err(e) = app.db.archive(&archived) {
        tracing::warn!("archiving tiles failed: {e:#}");
    }
}

/// Every live Claude Code session — interactive and background — as
/// `claude agents --json` reports it, by session id, with when it was asked:
/// what it reports is as of then, not of when it answers, so a hook landing
/// while it runs stays newer. `None` when the CLI is missing or answers
/// nothing usable; the cards then fall back to the hooks.
async fn live_agents(program: &str) -> Option<(i64, HashMap<String, Value>)> {
    let asked_ms = now_ms();
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(program).args(["agents", "--json"]).env("EMBARCH_DESK_INTERNAL", "1").output(),
    )
    .await
    .ok()?
    .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let agents = v
        .as_array()?
        .iter()
        .filter_map(|a| a["sessionId"].as_str().map(|id| (id.to_string(), a.clone())))
        .collect();
    Some((asked_ms, agents))
}

pub async fn sync_loop(app: Shared) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tick.tick().await;
        sync_once(&app, false).await;
        housekeep(&app, now_ms());
    }
}

/// Resolves once the desk is told to stop — Ctrl-C at a terminal, or the
/// SIGTERM systemd sends on every stop and restart — and saves the state on
/// the way out, so a deploy drops none of the last two seconds' hooks. The
/// SIGTERM handler is in place when this returns, not when it is first
/// polled.
pub fn save_on_stop(app: Shared) -> std::io::Result<impl std::future::Future<Output = ()>> {
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    Ok(async move {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        if let Err(e) = app.db.flush() {
            tracing::warn!("saving state on the way out failed: {e:#}");
        }
    })
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
        let v = view_json(&st, &Cache::default(), D0 + 12 * H, 7, None, &UtcDays);
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
        let v = view_json(&st, &Cache::default(), D0 + H, 7, None, &UtcDays);
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
        let v = view_json(&st, &Cache::default(), D0 + 2 * H, 7, None, &UtcDays);
        assert_eq!(v["drafts"].as_array().unwrap().len(), 1);
        assert_eq!(v["timer"]["item_id"], a.as_str());
        assert_eq!(v["timer"]["seconds"], 3600);
        assert_eq!(v["cards"][0]["sessions"]["total"], 0, "the timer is not counted as a Claude session");
        assert_eq!(v["sessions_waiting"], 0);
    }

    #[test]
    fn an_open_desk_scopes_the_cards_and_the_hand_holds_every_desks_unsent_tiles() {
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
        let v = view_json(&st, &Cache::default(), D0 + 11 * H, 7, None, &UtcDays);
        let cards: Vec<&str> = v["cards"].as_array().unwrap().iter().map(|c| c["title"].as_str().unwrap()).collect();
        assert_eq!(cards, ["on alpha"]);
        assert_eq!(v["hand"][0]["item_title"], "on beta");
        assert_eq!(v["hand"][0]["desk"], "Beta");
        assert_eq!(v["hand"][0]["item_id"], wb.as_str());
        assert_eq!(v["open_desk"]["name"], "Alpha");
        st.open_desk = None;
        let all = view_json(&st, &Cache::default(), D0 + 11 * H, 7, None, &UtcDays);
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
    fn a_tile_proposes_only_the_gap_and_the_day_merges_harvest_entries() {
        let mut st = State::default();
        let issue = store::IssueRef { key: "ABC-1".into(), ..Default::default() };
        let w = st.create_item("Bootload", Some(issue), D0);
        let mut s1 = crate::store::testing::session("s");
        s1.item_id = Some(w.clone());
        crate::store::record_activity(&mut s1, D0 + 9 * H, &UtcDays);
        crate::store::record_activity(&mut s1, D0 + 12 * H, &UtcDays);
        st.sessions.insert("s".into(), s1);
        let entry = |id, key: Option<&str>, hours, notes: &str| store::LoggedEntry {
            id, issue_key: key.map(str::to_string), hours, notes: notes.into(), project: "P".into(), task: "T".into(), ..Default::default()
        };
        st.harvest_logged.insert("2026-10-22".into(), vec![
            entry(1, Some("ABC-1"), 2.0, "ABC-1: bring-up"),
            entry(2, None, 0.5, "Weekly sync"),
        ]);
        let t = tile_json(&st, &w, "2026-10-22");
        assert_eq!((t["captured_hours"].as_f64(), t["logged_hours"].as_f64(), t["hours"].as_f64()), (Some(3.0), Some(2.0), Some(1.0)));
        let d = day_json(&st, "2026-10-22", D0 + 13 * H);
        let rows: Vec<&Value> = d["groups"].as_array().unwrap().iter().flat_map(|g| g["items"].as_array().unwrap()).collect();
        let item = rows.iter().find(|r| r["title"] == "Bootload").unwrap();
        assert_eq!(item["logged_hours"], 2.0);
        assert_eq!(item["harvest_entries"][0]["id"], 1);
        assert!(rows.iter().any(|r| r["title"] == "Weekly sync" && r["status"] == "harvest_only"));
        assert_eq!(d["total_hours"], 3.5, "1h unsent + 2.5h already in Harvest");
    }

    #[test]
    fn the_hand_puts_the_open_desks_tiles_first_then_oldest_first() {
        let mut st = State::default();
        let a = st.put_desk(store::Desk { name: "Alpha".into(), ..Default::default() }, D0);
        let b = st.put_desk(store::Desk { name: "Beta".into(), ..Default::default() }, D0);
        let issue = store::IssueRef { key: "ABC-1".into(), ..Default::default() };
        let wa = st.create_item("on alpha", Some(issue), D0);
        let wb = st.create_item("on beta", None, D0);
        st.items.get_mut(&wa).unwrap().desk_id = Some(a.clone());
        st.items.get_mut(&wb).unwrap().desk_id = Some(b.clone());
        for (sid, w, day) in [("s1", &wb, 0), ("s2", &wa, 1), ("s3", &wa, 0)] {
            let mut s = crate::store::testing::session(sid);
            s.item_id = Some(w.clone());
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 9 * H, &UtcDays);
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 10 * H, &UtcDays);
            st.sessions.insert(sid.into(), s);
        }
        st.open_desk = Some(a.clone());
        let v = view_json(&st, &Cache::default(), D0 + 35 * H, 7, None, &UtcDays);
        let hand: Vec<(&str, &str)> = v["hand"].as_array().unwrap().iter()
            .map(|t| (t["item_title"].as_str().unwrap(), t["date"].as_str().unwrap())).collect();
        assert_eq!(hand, [("on alpha", "2026-10-22"), ("on alpha", "2026-10-23"), ("on beta", "2026-10-22")]);
        assert_eq!(v["hand"][0]["key"], "ABC-1");
        st.items.get_mut(&wb).unwrap().archived = true;
        let v = view_json(&st, &Cache::default(), D0 + 35 * H, 7, None, &UtcDays);
        assert_eq!(v["hand"].as_array().unwrap().len(), 2, "an archived card's tiles leave the hand");
    }

    #[test]
    fn a_tile_harvest_already_covers_closes_itself_and_opens_again_when_the_entry_goes() {
        let mut st = State::default();
        let issue = store::IssueRef { key: "ABC-1".into(), ..Default::default() };
        let w = st.create_item("Bootload", Some(issue), D0);
        let mut s1 = crate::store::testing::session("s");
        s1.item_id = Some(w.clone());
        crate::store::record_activity(&mut s1, D0 + 9 * H, &UtcDays);
        crate::store::record_activity(&mut s1, D0 + 10 * H, &UtcDays);
        st.sessions.insert("s".into(), s1);
        st.harvest_logged.insert("2026-10-22".into(), vec![store::LoggedEntry {
            id: 1, issue_key: Some("ABC-1".into()), hours: 4.0, notes: "ABC-1: bring-up".into(), ..Default::default()
        }]);
        assert_eq!(tile_json(&st, &w, "2026-10-22")["covered"], true);
        let v = view_json(&st, &Cache::default(), D0 + 11 * H, 7, None, &UtcDays);
        assert_eq!(v["hand"].as_array().unwrap().len(), 0, "{v}");
        assert_eq!(v["cards"][0]["tiles"].as_array().unwrap().len(), 0);
        st.tile_mut(&w, "2026-10-22").hours_override = Some(0.5);
        assert_eq!(tile_json(&st, &w, "2026-10-22")["covered"], false, "hours set by hand are the user's call");
        st.tile_mut(&w, "2026-10-22").hours_override = None;
        st.harvest_logged.clear();
        let v = view_json(&st, &Cache::default(), D0 + 11 * H, 7, None, &UtcDays);
        assert_eq!(v["hand"][0]["hours"], 1.0);
    }

    #[test]
    fn a_tile_whose_entry_goes_reopens_on_the_current_gap_and_a_typed_override_is_kept_or_cleared() {
        let mut st = State::default();
        let issue = store::IssueRef { key: "ABC-1".into(), ..Default::default() };
        let w = st.create_item("Bootload", Some(issue), D0);
        let mut s1 = crate::store::testing::session("s");
        s1.item_id = Some(w.clone());
        crate::store::record_activity(&mut s1, D0 + 9 * H, &UtcDays);
        crate::store::record_activity(&mut s1, D0 + 10 * H, &UtcDays);
        st.sessions.insert("s".into(), s1);
        // The state approve_tile leaves: sent hours, no override.
        let t = st.tile_mut(&w, "2026-10-22");
        t.status = TileStatus::Approved;
        t.harvest_entry_id = Some(7);
        t.final_seconds = Some(3600);
        t.sent_hours = Some(1.0);
        st.harvest_logged.insert("2026-10-22".into(), vec![store::LoggedEntry {
            id: 7, issue_key: Some("ABC-1".into()), hours: 1.0, notes: "x".into(), ..Default::default()
        }]);
        assert_eq!(tile_json(&st, &w, "2026-10-22")["hours"], 1.0, "an approved tile reads what it sent");
        // More session time, then the entry is deleted.
        let mut s = st.sessions.remove("s").unwrap();
        crate::store::record_activity(&mut s, D0 + 11 * H, &UtcDays);
        st.sessions.insert("s".into(), s);
        st.harvest_logged.clear();
        st.reopen_tiles_of_entry(7);
        let j = tile_json(&st, &w, "2026-10-22");
        assert_eq!(j["status"], "open");
        assert_eq!(j["hours"], 2.0, "the reopened tile proposes the current gap, not the deleted entry's hours");
        // Harvest covers it again: it closes itself.
        st.harvest_logged.insert("2026-10-22".into(), vec![store::LoggedEntry {
            id: 8, issue_key: Some("ABC-1".into()), hours: 2.0, notes: "y".into(), ..Default::default()
        }]);
        assert_eq!(tile_json(&st, &w, "2026-10-22")["covered"], true);
        st.harvest_logged.clear();
        // A typed override survives a reopen.
        let t = st.tile_mut(&w, "2026-10-22");
        t.hours_override = Some(0.5);
        t.status = TileStatus::Approved;
        t.harvest_entry_id = Some(9);
        t.sent_hours = Some(0.5);
        st.reopen_tiles_of_entry(9);
        assert_eq!(tile_json(&st, &w, "2026-10-22")["hours"], 0.5);
        // PATCH null clears it; absent leaves it.
        let p: TilePatch = serde_json::from_str(r#"{"note":"n"}"#).unwrap();
        assert_eq!(p.hours, None);
        let p: TilePatch = serde_json::from_str(r#"{"hours":null}"#).unwrap();
        assert_eq!(p.hours, Some(None));
        let p: TilePatch = serde_json::from_str(r#"{"hours":1.5}"#).unwrap();
        assert_eq!(p.hours, Some(Some(1.5)));
        if let Some(h) = Some(None::<f64>) {
            st.tile_mut(&w, "2026-10-22").hours_override = h;
        }
        assert_eq!(tile_json(&st, &w, "2026-10-22")["hours"], 2.0);
    }

    #[test]
    fn past_days_with_waiting_tiles_are_read_once_each_per_interval_and_a_moved_gap_refuses_an_override() {
        let mut st = State::default();
        for (n, title) in ["a", "b", "c"].iter().enumerate() {
            let w = st.create_item(title, None, D0);
            for day in ["2026-10-21", "2026-10-22"] {
                if n < 2 || day == "2026-10-22" {
                    st.tile_mut(&w, day);
                }
            }
            st.tile_mut(&w, "2026-10-23");
        }
        let done = st.create_item("done", None, D0);
        st.tile_mut(&done, "2026-10-20").status = TileStatus::Approved;
        let mut fetched = HashMap::new();
        let now = D0 + 100 * H;
        let due = past_days_due(&st, &fetched, "2026-10-23", now);
        assert_eq!(due, ["2026-10-21", "2026-10-22"], "one read per distinct earlier day, not per tile, and not today or an acted-on tile's day");
        fetched.insert("2026-10-21".to_string(), now - 60_000);
        assert_eq!(past_days_due(&st, &fetched, "2026-10-23", now), ["2026-10-22"], "a fresh read is left alone");
        fetched.insert("2026-10-21".to_string(), now - PAST_DAY_REFRESH_MS - 1);
        assert_eq!(past_days_due(&st, &fetched, "2026-10-23", now).len(), 2);
        assert!(gap_moved_under_override(2.0, 1.0, Some(2.0)));
        assert!(!gap_moved_under_override(2.0, 2.0, Some(2.0)));
        assert!(!gap_moved_under_override(2.0, 1.0, None), "with no override ✓ sends the fresh gap");
    }

    #[test]
    fn a_folded_tile_sends_its_time_and_lines_with_the_later_one() {
        let mut st = State::default();
        let w = st.create_item("Comms", None, D0);
        for (sid, day, hours) in [("s1", 0, 2), ("s2", 2, 3)] {
            let mut s = crate::store::testing::session(sid);
            s.item_id = Some(w.clone());
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 9 * H, &UtcDays);
            crate::store::record_activity(&mut s, D0 + day * 24 * H + (9 + hours) * H, &UtcDays);
            st.sessions.insert(sid.into(), s);
        }
        st.add_line(&w, "2026-10-22", "read the contract", "agent", D0 + 10 * H);
        st.add_line(&w, "2026-10-24", "wrote the link", "agent", D0 + 58 * H);
        st.harvest_logged.insert("2026-10-22".into(), vec![store::LoggedEntry { id: 1, hours: 0.5, notes: "x".into(), ..Default::default() }]);
        st.items.get_mut(&w).unwrap().issue = Some(store::IssueRef { key: "ABC-1".into(), ..Default::default() });
        st.harvest_logged.get_mut("2026-10-22").unwrap()[0].issue_key = Some("ABC-1".into());
        assert!(st.fold_tile(&w, "2026-10-22", "2026-10-22", D0).is_err());
        assert!(st.fold_tile(&w, "2026-10-22", "2026-10-23", D0).is_err(), "no tile that day");
        st.fold_tile(&w, "2026-10-22", "2026-10-24", D0 + 60 * H).unwrap();
        let v = view_json(&st, &Cache::default(), D0 + 60 * H, 7, None, &UtcDays);
        let hand = v["hand"].as_array().unwrap();
        assert_eq!(hand.len(), 1, "{v}");
        assert_eq!((hand[0]["date"].as_str(), hand[0]["hours"].as_f64()), (Some("2026-10-24"), Some(4.5)), "the half hour Harvest held is not carried");
        let notes: Vec<String> = st.day_notes(&w, "2026-10-24", &UtcDays).into_iter().map(|l| l.text).collect();
        assert_eq!(notes, ["read the contract", "wrote the link"]);
        assert_eq!(hand[0]["lines"].as_array().unwrap().len(), 2, "the card shows the lines it will send");
        assert!(st.fold_tile(&w, "2026-10-22", "2026-10-24", D0).is_err(), "already folded");
        let d = day_json(&st, "2026-10-22", D0 + 60 * H);
        assert_eq!(d["groups"][0]["items"][0]["folded_into"], "2026-10-24");
        assert_eq!(d["unsent_hours"], 0.0);
        // A week on, the folded tile stays while its target is open.
        st.prune(D0 + 60 * H + store::ARCHIVE_AFTER_MS + H);
        assert_eq!(tile_json(&st, &w, "2026-10-24")["hours"], 4.5);
    }

    #[test]
    fn a_fold_goes_only_to_a_later_real_day_and_a_tile_id_names_a_real_day() {
        let mut st = State::default();
        let w = st.create_item("Comms", None, D0);
        for (sid, day) in [("s1", 0), ("s2", 2)] {
            let mut s = crate::store::testing::session(sid);
            s.item_id = Some(w.clone());
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 9 * H, &UtcDays);
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 10 * H, &UtcDays);
            st.sessions.insert(sid.into(), s);
        }
        for bad_into in ["2026-10-22", "2026-10-20", "2026-10-24x", "later", "2026-1-24", "2026-13-24", ""] {
            assert!(st.fold_tile(&w, "2026-10-22", bad_into, D0).is_err(), "{bad_into}");
        }
        assert!(st.fold_tile(&w, "2026-10-24", "2026-10-22", D0).is_err(), "an earlier day");
        assert!(st.fold_tile(&w, "nope", "2026-10-24", D0).is_err());
        assert!(st.tiles.values().all(|t| t.status == TileStatus::Open), "nothing changed");
        assert!(split_tile_id("w1@2026-10-22").is_ok());
        for id in ["w1", "w1@tomorrow", "w1@2026-02-30", "w1@", "w1@2026-1-2"] {
            assert!(split_tile_id(id).is_err(), "{id}");
        }
    }

    #[test]
    fn a_folded_day_reports_the_hours_it_carried_not_less_the_harvest_hours_twice() {
        let mut st = State::default();
        let w = st.create_item("Comms", Some(store::IssueRef { key: "ABC-1".into(), ..Default::default() }), D0);
        for (sid, day, hours) in [("s1", 0, 3), ("s2", 2, 1)] {
            let mut s = crate::store::testing::session(sid);
            s.item_id = Some(w.clone());
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 9 * H, &UtcDays);
            crate::store::record_activity(&mut s, D0 + day * 24 * H + (9 + hours) * H, &UtcDays);
            st.sessions.insert(sid.into(), s);
        }
        st.harvest_logged.insert("2026-10-22".into(), vec![store::LoggedEntry {
            id: 1, issue_key: Some("ABC-1".into()), hours: 0.5, notes: "x".into(), ..Default::default()
        }]);
        st.fold_tile(&w, "2026-10-22", "2026-10-24", D0 + 60 * H).unwrap();
        let t = tile_json(&st, &w, "2026-10-22");
        assert_eq!((t["hours"].as_f64(), t["captured_hours"].as_f64(), t["logged_hours"].as_f64()), (Some(2.5), Some(2.5), Some(0.5)), "{t}");
        assert_eq!(t["covered"], false);
        let d = day_json(&st, "2026-10-22", D0 + 60 * H);
        let row = &d["groups"][0]["items"][0];
        assert_eq!((row["hours"].as_f64(), row["captured_hours"].as_f64()), (Some(2.5), Some(2.5)), "{row}");
        assert_eq!(tile_json(&st, &w, "2026-10-24")["hours"], 3.5, "the target carries it once");
    }

    #[test]
    fn the_approve_note_falls_back_to_the_lines_of_the_folded_tiles_too() {
        let mut st = State::default();
        let w = st.create_item("Comms", None, D0);
        for (sid, day) in [("s1", 0), ("s2", 2)] {
            let mut s = crate::store::testing::session(sid);
            s.item_id = Some(w.clone());
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 9 * H, &UtcDays);
            crate::store::record_activity(&mut s, D0 + day * 24 * H + 10 * H, &UtcDays);
            st.sessions.insert(sid.into(), s);
        }
        st.add_line(&w, "2026-10-22", "read the contract", "agent", D0 + 10 * H);
        st.fold_tile(&w, "2026-10-22", "2026-10-24", D0 + 60 * H).unwrap();
        assert!(st.tiles[&format!("{w}@2026-10-24")].lines.is_empty(), "the target has none of its own");
        assert_eq!(approve_fallback_note(&st, &w, "2026-10-24"), "read the contract");
    }

    #[test]
    fn a_cards_live_sessions_carry_state_last_reply_and_both_links() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        let mut s1 = crate::store::testing::session("abc-123");
        s1.item_id = Some(w.clone());
        s1.cwd = "/w/fw-abc-1".into();
        s1.last_reply = Some("Here is the summary.".into());
        crate::store::record_activity(&mut s1, D0 + H, &UtcDays);
        st.sessions.insert("abc-123".into(), s1);
        let mut cache = Cache { wsl_distro: Some("Ubuntu".into()), agents_ms: D0 + 2 * H, ..Default::default() };
        cache.agents.insert("abc-123".into(), json!({"sessionId": "abc-123", "status": "waiting", "waitingFor": "permission prompt", "name": "fw-abc-1"}));
        let v = view_json(&st, &cache, D0 + 2 * H, 7, None, &UtcDays);
        let row = &v["cards"][0]["live_sessions"][0];
        assert_eq!((row["status"].as_str(), row["waiting_for"].as_str()), (Some("waiting"), Some("permission prompt")));
        assert_eq!(row["answer_url"], "https://claude.ai/code/abc-123");
        assert_eq!(row["session_uri"], "vscode://anthropic.claude-code/open?session=abc-123");
        assert_eq!(row["folder_uri"], "vscode://vscode-remote/wsl+Ubuntu/w/fw-abc-1");
        assert_eq!(row["last_reply"], "Here is the summary.");
    }

    #[test]
    fn the_start_prompt_names_the_issue_the_tool_and_the_branch() {
        let issue = store::IssueRef { key: "ABC-7".into(), title: "Loader".into(), url: "https://linear.app/x/issue/ABC-7".into(), ..Default::default() };
        let p = start_prompt(&issue, "w9", "user/abc-7-loader");
        assert!(p.contains("ABC-7") && p.contains("linear_get_issue") && p.contains("user/abc-7-loader") && p.contains("wait for my first instruction"));
    }

    #[test]
    fn the_polish_prompt_carries_lines_targets_and_the_reply_shape() {
        let lines = vec![store::Line::new(0, "smp: retry a busy upload", "commit")];
        let targets = vec![harvest::Target {
            account_id: 1, account_name: "a".into(), project_id: 2, project_name: "Fw".into(),
            client_name: "C".into(), task_id: 3, task_name: "Dev".into(),
        }];
        let p = polish_prompt("ABC-123 Bootload", "2026-10-22", 2.5, &lines, &targets, &[]);
        assert!(p.contains("smp: retry a busy upload") && p.contains("1 / 2 / 3: C / Fw / Dev") && p.contains("\"note\""));
    }

    #[test]
    fn the_polish_reads_the_days_notes_of_every_kind() {
        let mut st = State::default();
        let w = st.create_item("Loader", None, D0);
        st.add_line(&w, "2026-10-22", "smp: retry a busy upload", "commit", D0 + H);
        let mut d = store::Line::new(D0 + 2 * H, "keep the loader in RRAM", "agent");
        d.kind = "decision".into();
        st.add_note(&w, "2026-10-22", d);
        st.add_note(&w, "2026-10-22", store::Line::memory(D0 + 3 * H, "/h/.claude/projects/-w/memory/loader.md", "# Loader\n\nbranch user/abc-1,\nPR open"));
        st.add_line(&w, "2026-10-23", "the next day", "agent", D0 + 25 * H);
        let p = polish_prompt("Loader", "2026-10-22", 3.0, &st.day_notes(&w, "2026-10-22", &UtcDays), &[], &[]);
        assert!(p.contains("- [commit] smp: retry a busy upload\n- [decision] keep the loader in RRAM\n- [memory file loader.md, as it stood] # Loader branch user/abc-1, PR open\n"), "{p}");
        assert!(!p.contains("the next day"));
    }

    #[test]
    fn the_hooks_and_claude_agents_join_on_whichever_saw_the_session_last() {
        let mut s = crate::store::testing::session("s");
        s.last_ms = D0 + 10;
        s.busy = true;
        let mut cache = Cache::default();
        cache.agents.insert("s".into(), json!({"sessionId": "s", "status": "waiting", "waitingFor": "permission prompt"}));
        cache.agents_ms = D0;
        assert_eq!(session_state(&cache, &s), ("busy", None), "a hook after the read wins");
        cache.agents_ms = D0 + 20;
        assert_eq!(session_state(&cache, &s), ("waiting", Some("permission prompt".into())));
        s.busy = false;
        s.waiting = Some("your reply".into());
        assert_eq!(session_state(&Cache::default(), &s), ("waiting", Some("your reply".into())));
        s.waiting = None;
        assert_eq!(session_state(&Cache::default(), &s), ("idle", None));
        s.ended = true;
        assert_eq!(session_state(&Cache::default(), &s), ("ended", None));
    }

    #[test]
    fn the_view_lists_every_live_session_with_its_state_and_client_but_not_the_fleets() {
        let mut st = State { ignore: vec!["/g/embarch".into()], ..Default::default() };
        let w = st.create_item("x", None, D0);
        let mut chat = crate::store::testing::session("chat");
        chat.item_id = Some(w.clone());
        chat.client = Some("embarch-chat".into());
        chat.busy = true;
        chat.last_ms = D0 + H;
        let mut fleet = crate::store::testing::session("fleet");
        fleet.cwd = "/g/embarch/embarch-ui".into();
        fleet.ignored = true;
        fleet.last_ms = D0 + H;
        let mut mine = crate::store::testing::session("untracked");
        mine.ignored = true;
        mine.last_ms = D0 + H;
        let mut old = crate::store::testing::session("old");
        old.last_ms = D0 - 2 * H;
        for s in [chat, fleet, mine, old] {
            st.sessions.insert(s.id.clone(), s);
        }
        let v = view_json(&st, &Cache::default(), D0 + H + 1, 7, None, &UtcDays);
        let ids: Vec<&str> = v["sessions"].as_array().unwrap().iter().map(|s| s["session_id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&"chat") && ids.contains(&"untracked"));
        let chat = v["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == "chat").unwrap();
        assert_eq!((chat["state"].as_str(), chat["client"].as_str(), chat["item_id"].as_str()), (Some("busy"), Some("embarch-chat"), Some(w.as_str())));
        assert_eq!(chat["last_ms"], D0 + H);
        let row = &v["cards"][0]["live_sessions"][0];
        assert_eq!((row["state"].as_str(), row["client"].as_str(), row["cwd"].as_str()), (Some("busy"), Some("embarch-chat"), Some("/w/repo")));
        assert_eq!(v["settings"]["chat_panel"], false);
    }

    fn scratch_app() -> (Shared, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("desk-app-{}", crate::paths::random_hex(4).unwrap()));
        let app = Arc::new(App {
            db: Db::open(dir.clone()).unwrap(),
            refs: refs::Store::open(&dir),
            token: "t".into(),
            cache: Mutex::new(Cache::default()),
            wsl_distro: None,
            window_days: 7,
            model: String::new(),
        });
        (app, dir)
    }

    #[tokio::test]
    async fn the_mailbox_takes_a_focus_and_a_start_only_from_start_work() {
        let (app, dir) = scratch_app();
        let w = app.db.write(|st| st.create_item("x", None, D0));
        let post = |b: Value| post_mailbox(Ax(app.clone()), Json(serde_json::from_value(b).unwrap()));
        // A page that reaches the desk must not get a prompt run in auto mode.
        let start = post(json!({"kind": "start", "item_id": w, "folder": "/w/fw-abc-1", "prompt": "run this"})).await;
        assert!(start.is_err());
        assert!(app.db.read(|st| st.mailbox.is_empty()));
        let Ok(Json(v)) = post(json!({"kind": "focus", "session_id": "s", "folder": "/w/fw-abc-1", "prompt": "ignored"})).await else { panic!("a focus is taken") };
        let m = app.db.read(|st| st.mailbox[0].clone());
        assert_eq!((v["id"].as_str(), m.kind.as_str(), m.session_id.as_deref(), m.prompt), (Some(m.id.as_str()), "focus", Some("s"), None));
        assert!(post(json!({"kind": "focus", "folder": "/w/fw-abc-1"})).await.is_err(), "a focus needs its session");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn reading_the_view_never_rewrites_the_state_and_the_sync_loop_ticks_a_timer() {
        let (app, dir) = scratch_app();
        let w = app.db.write(|st| st.create_item("x", None, D0));
        let saved = || {
            app.db.flush().unwrap();
            std::fs::remove_file(dir.join("state.json")).is_ok()
        };
        assert!(saved());
        assert!(view(Ax(app.clone()), Query(ViewQuery::default())).await.is_ok());
        assert!(!saved(), "a poll with nothing running changes nothing");
        let now = now_ms();
        app.db.write(|st| st.start_timer(&w, now, &LocalDays));
        assert!(saved());
        assert!(view(Ax(app.clone()), Query(ViewQuery::default())).await.is_ok());
        assert!(!saved(), "a running timer is not ticked by a poll either");
        housekeep(&app, now + 60_000);
        assert!(saved(), "the sync loop ticks it");
        let sid = app.db.read(|st| st.timer.as_ref().unwrap().session_id.clone());
        assert_eq!(app.db.read(|st| st.sessions[&sid].last_ms), now + 60_000);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_housekeeping_pass_that_removes_nothing_leaves_the_state_file_alone() {
        let (app, dir) = scratch_app();
        let w = app.db.write(|st| st.create_item("x", None, D0));
        app.db.flush().unwrap();
        std::fs::remove_file(dir.join("state.json")).unwrap();
        let now = now_ms();
        housekeep(&app, now);
        app.db.flush().unwrap();
        assert!(!dir.join("state.json").exists(), "a prune with nothing to do rewrote state.json");
        app.db.write(|st| {
            let t = st.tile_mut(&w, "2026-10-22");
            t.status = TileStatus::Approved;
            t.acted_ms = Some(now - 8 * 24 * H);
        });
        app.db.flush().unwrap();
        std::fs::remove_file(dir.join("state.json")).unwrap();
        housekeep(&app, now);
        app.db.flush().unwrap();
        assert!(dir.join("state.json").exists(), "archiving a tile is a change");
        assert!(dir.join("history/2026-10.jsonl").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn sigterm_stops_the_desk_and_saves_the_state() {
        let (app, dir) = scratch_app();
        app.db.write(|st| st.create_item("x", None, D0));
        let stop = save_on_stop(app.clone()).unwrap();
        // systemd stops and restarts the service with SIGTERM.
        let kill = std::process::Command::new("kill").args(["-TERM", &std::process::id().to_string()]).status().unwrap();
        assert!(kill.success());
        tokio::time::timeout(std::time::Duration::from_secs(5), stop).await.expect("SIGTERM stops the desk");
        assert!(dir.join("state.json").exists(), "the state is saved on the way out");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn claude_agents_is_dated_by_when_it_was_asked_not_when_it_answered() {
        let dir = std::env::temp_dir().join(format!("desk-agents-{}", crate::paths::random_hex(4).unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        let cli = dir.join("claude");
        std::fs::write(&cli, "#!/bin/sh\nsleep 0.4\necho '[{\"sessionId\": \"s\", \"status\": \"waiting\"}]'\n").unwrap();
        std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let before = now_ms();
        let (asked_ms, agents) = live_agents(cli.to_str().unwrap()).await.unwrap();
        assert!(now_ms() - before >= 400);
        // A hook that lands while the CLI runs is newer than its answer.
        assert!(asked_ms - before < 200, "{}", asked_ms - before);
        assert_eq!(agents["s"]["status"], "waiting");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_polish_keeps_the_days_newest_notes_when_there_are_too_many() {
        let lines: Vec<store::Line> = (0..70).map(|i| store::Line::new(i, &format!("note {i}"), "agent")).collect();
        let p = polish_prompt("Loader", "2026-10-22", 3.0, &lines, &[], &[]);
        assert!(p.contains("] note 69\n") && p.contains("] note 10\n"), "{p}");
        assert!(!p.contains("] note 9\n") && p.contains("10 earlier notes left out"), "{p}");
    }

    #[test]
    fn a_card_keeps_its_ended_chats_so_open_chat_can_resume_one() {
        let mut st = State::default();
        let now = D0 + 10 * 24 * H;
        let w = st.create_item("x", None, now - H);
        let mut add = |id: &str, client: Option<&str>, ended: bool, last_ms: i64| {
            let mut s = crate::store::testing::session(id);
            s.item_id = Some(w.clone());
            s.client = client.map(str::to_string);
            s.ended = ended;
            s.last_ms = last_ms;
            st.sessions.insert(id.into(), s);
        };
        add("live", None, false, now - 60_000);
        for i in 0..7 {
            add(&format!("chat{i}"), Some("embarch-chat"), true, now - (i + 1) * H);
        }
        add("past-window", Some("embarch-chat"), true, now - 8 * 24 * H);
        add("terminal", None, true, now - 30_000);
        let v = view_json(&st, &Cache::default(), now, 7, None, &UtcDays);
        let rows = v["cards"][0]["live_sessions"].as_array().unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r["session_id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["live", "chat0", "chat1", "chat2", "chat3", "chat4"], "the 5 newest ended chats in the window");
        assert_eq!((rows[1]["state"].as_str(), rows[1]["client"].as_str()), (Some("ended"), Some("embarch-chat")));
        assert_eq!(v["cards"][0]["sessions"]["live"], 1, "an ended chat is kept, not live");
    }

    #[test]
    fn a_resumed_session_whose_start_was_lost_stays_live_on_its_card() {
        let mut st = State::default();
        let git = crate::store::testing::FakeGit::default();
        let w = st.create_item("x", None, D0);
        let hook = |st: &mut State, name: &str, at: i64| {
            let ev: events::HookEvent = serde_json::from_value(json!({"hook_event_name": name, "session_id": "s", "cwd": "/w/r"})).unwrap();
            events::apply(st, &ev, at, &UtcDays, &git, 7);
        };
        hook(&mut st, "SessionStart", D0);
        st.assign_session("s", &w, D0, &UtcDays, &git);
        hook(&mut st, "SessionEnd", D0 + H);
        // Resumed while the desk restarted: no SessionStart.
        hook(&mut st, "UserPromptSubmit", D0 + 2 * H);
        hook(&mut st, "PostToolUse", D0 + 2 * H + 60_000);
        let v = view_json(&st, &Cache::default(), D0 + 2 * H + 61_000, 7, None, &UtcDays);
        assert_eq!(v["cards"][0]["sessions"]["live"], 1, "{v}");
        assert_eq!(v["cards"][0]["live_sessions"][0]["state"], "busy");
        // A hook newer than the last claude agents read says busy, not ended.
        let mut cache = Cache { agents_ms: D0 + 2 * H + 30_000, ..Default::default() };
        cache.agents.insert("s".into(), json!({"sessionId": "s", "status": "busy"}));
        let v = view_json(&st, &cache, D0 + 2 * H + 61_000, 7, None, &UtcDays);
        assert_eq!(v["cards"][0]["live_sessions"][0]["state"], "busy");
    }

    #[test]
    fn a_window_on_another_desk_reads_that_desks_cards() {
        let mut st = State::default();
        let a = st.put_desk(store::Desk { name: "Alpha".into(), ..Default::default() }, D0);
        let b = st.put_desk(store::Desk { name: "Beta".into(), ..Default::default() }, D0);
        for (title, desk) in [("on alpha", &a), ("on beta", &b)] {
            let w = st.create_item(title, None, D0);
            st.items.get_mut(&w).unwrap().desk_id = Some(desk.clone());
            st.add_line(&w, "2026-10-22", "did a thing", "agent", D0 + H);
        }
        st.open_desk = Some(a.clone());
        let titles = |v: &Value| v["cards"].as_array().unwrap().iter().map(|c| c["title"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(titles(&view_json(&st, &Cache::default(), D0 + 2 * H, 7, None, &UtcDays)), ["on alpha"]);
        let v = view_json(&st, &Cache::default(), D0 + 2 * H, 7, Some(b.as_str()), &UtcDays);
        assert_eq!(titles(&v), ["on beta"]);
        assert_eq!(v["open_desk"]["name"], "Alpha", "the open desk stays what it is");
    }

    #[tokio::test]
    async fn the_view_names_its_desk_or_answers_404() {
        let (app, dir) = scratch_app();
        let b = app.db.write(|st| st.put_desk(store::Desk { name: "Beta".into(), ..Default::default() }, D0));
        let q = |desk: &str| Query(ViewQuery { desk: Some(desk.into()) });
        assert!(view(Ax(app.clone()), q(&b)).await.is_ok());
        let Err(e) = view(Ax(app.clone()), q("k404")).await else { panic!("an unknown desk is not the open one") };
        assert_eq!(e.0, StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(dir).ok();
    }

    fn refs_index() -> (refs::Scope, refs::Fetched) {
        let scope = refs::Scope {
            linear: Some(store::LinearBinding { account: "work".into(), team_id: "t1".into(), team_key: "ABC".into(), ..Default::default() }),
            repos: vec!["owner/repo".into()],
        };
        let got = refs::Fetched {
            issues: Some(vec![refs::RefIssue {
                key: "ABC-12".into(),
                title: "Add the widget".into(),
                state: "Done".into(),
                url: "https://linear.app/ws/issue/ABC-12".into(),
            }]),
            prs: vec![(
                "owner/repo".into(),
                Some(vec![refs::RefPr {
                    repo: "owner/repo".into(),
                    number: 7,
                    title: "Widget".into(),
                    state: "merged".into(),
                    draft: false,
                    url: "https://github.com/owner/repo/pull/7".into(),
                    branch: "feature/abc-12-widget".into(),
                    keys: vec![],
                }]),
            )],
        };
        (scope, got)
    }

    async fn get_refs(app: &Shared, desk: Option<&str>) -> Result<Value, StatusCode> {
        match refs_route(Ax(app.clone()), Query(ViewQuery { desk: desk.map(str::to_string) })).await {
            Ok(Json(v)) => Ok(v),
            Err(e) => Err(e.0),
        }
    }

    async fn get_view(app: &Shared, desk: Option<&str>) -> Value {
        let Ok(Json(v)) = view(Ax(app.clone()), Query(ViewQuery { desk: desk.map(str::to_string) })).await else { panic!("view") };
        v
    }

    #[tokio::test]
    async fn refs_serves_the_open_or_named_desks_index_in_the_contracts_shape() {
        let (app, dir) = scratch_app();
        let a = app.db.write(|st| st.put_desk(store::Desk { name: "Alpha".into(), ..Default::default() }, D0));
        let b = app.db.write(|st| st.put_desk(store::Desk { name: "Beta".into(), ..Default::default() }, D0));
        assert_eq!(get_refs(&app, None).await.unwrap(), json!({"desk_id": null, "synced_ms": null, "issues": [], "prs": []}), "no desk open");
        app.db.write(|st| st.open_desk = Some(a.clone()));
        assert_eq!(get_refs(&app, None).await.unwrap(), json!({"desk_id": a, "synced_ms": null, "issues": [], "prs": []}), "empty before the first sync");
        assert_eq!(get_refs(&app, Some("k404")).await, Err(StatusCode::NOT_FOUND));
        let (scope, got) = refs_index();
        assert!(app.refs.apply(&b, &scope, got, D0 + H));
        let want = json!({
            "desk_id": b, "synced_ms": D0 + H,
            "issues": [{"key": "ABC-12", "title": "Add the widget", "state": "Done", "url": "https://linear.app/ws/issue/ABC-12"}],
            "prs": [{"repo": "owner/repo", "number": 7, "title": "Widget", "state": "merged", "draft": false,
                     "url": "https://github.com/owner/repo/pull/7", "branch": "feature/abc-12-widget", "keys": ["ABC-12"]}],
        });
        assert_eq!(get_refs(&app, Some(&b)).await.unwrap(), want);
        assert_eq!(get_refs(&app, Some("beta")).await.unwrap(), want, "a desk by its name, as /view takes it");
        assert_eq!(get_refs(&app, None).await.unwrap()["desk_id"], a.as_str(), "the open desk stays the default");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn the_view_carries_the_viewed_desks_refs_ms() {
        let (app, dir) = scratch_app();
        let a = app.db.write(|st| st.put_desk(store::Desk { name: "Alpha".into(), ..Default::default() }, D0));
        let b = app.db.write(|st| st.put_desk(store::Desk { name: "Beta".into(), ..Default::default() }, D0));
        assert_eq!(get_view(&app, None).await["refs_ms"], Value::Null, "no desk open");
        app.db.write(|st| st.open_desk = Some(a.clone()));
        assert_eq!(get_view(&app, None).await["refs_ms"], Value::Null, "before the first sync");
        let (scope, got) = refs_index();
        app.refs.apply(&b, &scope, got, D0 + H);
        assert_eq!(get_view(&app, None).await["refs_ms"], Value::Null, "Alpha has none yet");
        assert_eq!(get_view(&app, Some(&b)).await["refs_ms"], D0 + H);
        app.refs.apply(&a, &refs::Scope::default(), refs::Fetched::default(), D0 + 2 * H);
        assert_eq!(get_view(&app, None).await["refs_ms"], D0 + 2 * H);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_restarted_desk_serves_its_last_index_at_once() {
        let (app, dir) = scratch_app();
        let a = app.db.write(|st| st.put_desk(store::Desk { name: "Alpha".into(), ..Default::default() }, D0));
        app.db.write(|st| st.open_desk = Some(a.clone()));
        app.db.flush().unwrap();
        let (scope, got) = refs_index();
        app.refs.apply(&a, &scope, got, D0 + H);
        let before = get_refs(&app, None).await.unwrap();
        drop(app);
        let again = Arc::new(App {
            db: Db::open(dir.clone()).unwrap(),
            refs: refs::Store::open(&dir),
            token: "t".into(),
            cache: Mutex::new(Cache::default()),
            wsl_distro: None,
            window_days: 7,
            model: String::new(),
        });
        let after = get_refs(&again, None).await.unwrap();
        assert_eq!(after, before);
        assert_eq!(after["prs"][0]["keys"], json!(["ABC-12"]));
        assert_eq!(get_view(&again, None).await["refs_ms"], D0 + H);
        std::fs::remove_dir_all(dir).ok();
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        let wait = async {
            while !ok() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(5), wait).await.is_err() {
            panic!("timed out waiting for {what}");
        }
    }

    // What POST /sync does for the index while the task's pass hangs: it makes every desk due and wakes the
    // task, waiting on neither, and one more pass follows the hung one for the desk fetched before the wake.
    // That sync_once itself never refreshes is not tested here: it reads the real connectors (decision 56).
    #[tokio::test]
    async fn a_sync_wake_during_a_hung_reference_pass_reruns_it_after_for_the_desk_fetched_before() {
        let (app, dir) = scratch_app();
        let desk = |name: &str, repo: &str| store::Desk { name: name.into(), github_repos: vec![repo.into()], ..Default::default() };
        let a = app.db.write(|st| st.put_desk(desk("Alpha", "o/a"), D0));
        let b = app.db.write(|st| st.put_desk(desk("Beta", "o/b"), D0));
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let (open, gate) = tokio::sync::watch::channel(false);
        let pass = {
            let calls = calls.clone();
            move |app: Shared| {
                let (calls, gate) = (calls.clone(), gate.clone());
                async move {
                    refresh_refs(&app, |scope: refs::Scope| {
                        let first = {
                            let mut c = calls.lock().unwrap();
                            c.push(scope.repos.join(","));
                            c.len() == 1
                        };
                        let mut gate = gate.clone();
                        async move {
                            if first {
                                // A network that drops packets: the first fetch hangs until let go.
                                gate.wait_for(|o| *o).await.ok();
                            }
                            refs::Fetched { issues: None, prs: scope.repos.iter().map(|r| (r.clone(), Some(vec![]))).collect() }
                        }
                    })
                    .await
                }
            }
        };
        let task = tokio::spawn(refs_loop_with(app.clone(), std::time::Duration::from_secs(3600), pass));
        until("the first fetch", || calls.lock().unwrap().len() == 1).await;
        let hung = calls.lock().unwrap()[0].clone();
        let (first, second) = if hung == "o/a" { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
        assert!(!app.refs.claim(&first, now_ms()), "a desk is claimed before its fetch");
        // What POST /sync does for the index, while a pass hangs.
        app.refs.forget_checked(None);
        app.refs.wake();
        assert_eq!((app.refs.synced_ms(&a), app.refs.synced_ms(&b)), (None, None));
        open.send(true).unwrap();
        until("the pass and the one POST /sync asked for", || calls.lock().unwrap().len() == 3).await;
        until("the second desk's index", || app.refs.synced_ms(&second).is_some()).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let other = if hung == "o/a" { "o/b" } else { "o/a" };
        assert_eq!(*calls.lock().unwrap(), [hung.as_str(), other, hung.as_str()], "the desk fetched before POST /sync is fetched again, the other once, after it");
        assert!(app.refs.synced_ms(&first).is_some());
        task.abort();
        // Five minutes per desk still holds: a pass now fetches nothing.
        let fetched = Mutex::new(0);
        refresh_refs(&app, |_| {
            *fetched.lock().unwrap() += 1;
            async { refs::Fetched::default() }
        })
        .await;
        assert_eq!(*fetched.lock().unwrap(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_card_carries_when_its_notes_last_changed() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        let v = view_json(&st, &Cache::default(), D0 + H, 7, None, &UtcDays);
        assert_eq!(v["cards"][0]["notes_ms"], Value::Null);
        st.add_line(&w, "2026-10-22", "built it", "agent", D0 + H);
        st.add_note(&w, "", store::Line::memory(D0 + 2 * H, "/m/a.md", "state"));
        st.add_line(&w, "2026-10-21", "older day, added now", "user", D0 + 3 * H);
        let v = view_json(&st, &Cache::default(), D0 + 4 * H, 7, None, &UtcDays);
        assert_eq!(v["cards"][0]["notes_ms"], D0 + 3 * H);
        // A line held before its session had an item lands with its own,
        // older time; the card still moves.
        let mut s = crate::store::testing::session("b");
        s.held_lines.push(store::Line::new(D0 + H + 1, "held", "agent"));
        st.sessions.insert("b".into(), s);
        st.assign_session("b", &w, D0 + 5 * H, &UtcDays, &crate::store::testing::FakeGit::default());
        let v = view_json(&st, &Cache::default(), D0 + 5 * H, 7, None, &UtcDays);
        assert_eq!(v["cards"][0]["notes_ms"], D0 + 5 * H);
    }

    #[test]
    fn a_view_tile_line_names_its_fields_as_notes_does() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        st.add_line(&w, "2026-10-22", "built it", "agent", D0 + H);
        let v = view_json(&st, &Cache::default(), D0 + 2 * H, 7, None, &UtcDays);
        let note = brief::note_json(st.item_notes(&w)[0], true, &UtcDays);
        for line in [&v["cards"][0]["tiles"][0]["lines"][0], &v["day"]["groups"][0]["items"][0]["lines"][0]] {
            assert_eq!((&line["id"], &line["ts_ms"]), (&note["id"], &note["ts_ms"]), "{line}");
            assert!(!note["id"].as_str().unwrap().is_empty());
            assert_eq!(line["ms"], D0 + H, "kept for the clients that read it");
        }
    }

    #[test]
    fn a_start_entry_carries_the_item_the_worktree_and_the_prompt() {
        let mut st = State::default();
        let m = st.post_mail(start_entry("w3", "/w/fw-abc-7", "Read ABC-7"), D0);
        let got = st.take_mail(&m, D0 + 1).unwrap();
        assert_eq!((got.kind.as_str(), got.item_id.as_deref(), got.folder.as_str(), got.prompt.as_deref()), ("start", Some("w3"), "/w/fw-abc-7", Some("Read ABC-7")));
        assert_eq!(got.expires_ms, D0 + store::LAUNCH_CLAIM_MS);
    }
}
