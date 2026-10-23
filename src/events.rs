//! What a Claude Code hook event does to the desk.
//!
//! The plugin's hooks forward their stdin JSON here verbatim. Every event of
//! a tracked session extends its time; a few do more:
//!
//! - **SessionStart** claims a pending "open in VS Code" launch, or a work
//!   item whose issue key the branch carries, and tells the session its desk
//!   id — the tools need it, and nothing else identifies a session to them.
//! - **UserPromptSubmit** on a session with no work item asks, every prompt,
//!   until it is answered (proposal decision 7).
//! - **PostToolUse** turns a `git commit` into a tile line and an
//!   `embarch-api` build, flash or study into bench activity.
//!
//! A session under an ignored path is dropped at the door: no time, no
//! question, no context.

use crate::store::{self, BenchEvent, Days, Git, Line, Session, State};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct HookEvent {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    #[serde(default)]
    pub tool_response: Option<Value>,
    /// Stop: the text of Claude's final response.
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// Notification: `permission_prompt`, `idle_prompt`, …
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// The `embarch-api` tools that count as bench activity. Anything else it
/// exposes (listing, status) is a read, not work on the bench.
const BENCH_TOOLS: &[&str] = &[
    "build",
    "flash",
    "build_and_flash",
    "bootload",
    "build_and_bootload",
    "run_study",
    "reset",
    "build_dev_bench",
    "flash_dev_bench",
    "build_and_flash_dev_bench",
];

/// `mcp__embarch-api__flash` → `flash`; `None` for anything not embarch-api's.
pub fn bench_tool(tool_name: &str) -> Option<&str> {
    let rest = tool_name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (server.contains("embarch-api") && BENCH_TOOLS.contains(&tool)).then_some(tool)
}

fn tool_failed(resp: &Option<Value>) -> bool {
    let Some(v) = resp else { return false };
    v.get("isError").or_else(|| v.get("is_error")).and_then(Value::as_bool).unwrap_or(false)
}

fn is_git_commit(input: &Option<Value>) -> bool {
    input
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("git") && c.contains("commit"))
}

/// The line a session is shown when it has no work item.
pub fn ask_text(state: &State, session_id: &str, now: i64, window_days: i64) -> String {
    // The session's own desk first, else the open one: the cards it is most
    // likely to be.
    let scope = state
        .sessions
        .get(session_id)
        .and_then(|s| s.desk_id.clone())
        .or_else(|| state.open_desk.clone());
    let mut items: Vec<_> = state
        .items
        .values()
        .filter(|i| !i.archived && (i.pinned || now - i.last_activity_ms < window_days * 86_400_000))
        .filter(|i| scope.is_none() || i.desk_id == scope)
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.last_activity_ms));
    let list: Vec<String> = items
        .iter()
        .take(8)
        .map(|i| match &i.issue {
            Some(r) => format!("{} ({}: {})", i.id, r.key, i.title),
            None => format!("{} ({})", i.id, i.title),
        })
        .collect();
    let choices = if list.is_empty() {
        "no cards are open yet".to_string()
    } else {
        format!("open cards: {}", list.join("; "))
    };
    format!(
        "EmbArch Desk: this session (desk session id `{session_id}`) is not on a work item, so \
         its time has nowhere to land. Before anything else, ask the user in one line which work \
         item it is — {choices} — or a Linear issue key, or a title for a new standalone item. \
         Then call the `desk_set_work_item` tool with session_id `{session_id}` and the answer \
         (a new standalone item also needs its desk — ask which). If the user says this is not \
         tracked work, call it with untracked: true. Ask again on every prompt until it is answered."
    )
}

fn assigned_text(state: &State, session_id: &str, item_id: &str) -> String {
    let label = state
        .items
        .get(item_id)
        .map(|i| match &i.issue {
            Some(r) => format!("{} {} ({})", r.key, r.title, r.url),
            None => i.title.clone(),
        })
        .unwrap_or_else(|| item_id.to_string());
    format!(
        "EmbArch Desk: desk session id `{session_id}`; this session is on work item {item_id}: \
         {label}. When you finish a piece of work that has no commit — an investigation's \
         finding, a flash or study result — add one short line to its time tile with the \
         `desk_add_line` tool. Commits are added automatically."
    )
}

/// When a session starts in one desk's repo while another (or none) is open:
/// the one-time question, and the rule that its time is its own desk's.
fn switch_text(state: &State, session_id: &str) -> Option<String> {
    let sess = state.sessions.get(session_id)?;
    let own = sess.desk_id.as_ref()?;
    if state.open_desk.as_ref() == Some(own) {
        return None;
    }
    let name = |id: &String| state.desks.get(id).map(|d| d.name.clone()).unwrap_or_else(|| id.clone());
    let open = state.open_desk.as_ref().map(name).unwrap_or_else(|| "no desk".to_string());
    Some(format!(
        "EmbArch Desk: this repo belongs to the desk \"{}\", but \"{open}\" is open. Ask the user \
         once whether to switch the open desk to \"{}\" (the `desk_open` tool) — that also moves \
         the UI, the agents' Linear and GitHub scope and the default build target, and proposes \
         its bench. Either way, this session's time goes to \"{}\".",
        name(own), name(own), name(own)
    ))
}

/// Apply one hook event. Returns the context to inject into the session, if any.
pub fn apply(
    state: &mut State,
    ev: &HookEvent,
    now: i64,
    days: &dyn Days,
    git: &dyn Git,
    window_days: i64,
) -> Option<String> {
    if ev.session_id.is_empty() {
        return None;
    }
    let ignore = state.ignore.clone();
    let desk_here = state.desk_for_path(&ev.cwd);
    let sess = state.sessions.entry(ev.session_id.clone()).or_insert_with(|| Session {
        id: ev.session_id.clone(),
        cwd: ev.cwd.clone(),
        desk_id: desk_here,
        item_id: None,
        ignored: store::is_ignored(&ev.cwd, &ignore),
        started_ms: now,
        last_ms: now,
        spans: vec![],
        commits: vec![],
        held_lines: vec![],
        held_bench: vec![],
        ended: false,
        last_reply: None,
        last_reply_ms: 0,
        waiting: None,
    });
    if sess.ignored {
        return None;
    }
    // What the card's session row shows: the last reply, and whether it
    // waits on the user.
    match ev.hook_event_name.as_str() {
        "Stop" => {
            if let Some(m) = ev.last_assistant_message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                sess.last_reply = Some(m.chars().take(800).collect());
                sess.last_reply_ms = now;
            }
            sess.waiting = None;
        }
        "Notification" => {
            sess.waiting = Some(match (ev.notification_type.as_deref(), ev.message.as_deref()) {
                (Some("permission_prompt"), _) => "permission prompt".to_string(),
                (Some("idle_prompt"), _) => "your reply".to_string(),
                (_, Some(m)) if !m.is_empty() => m.chars().take(120).collect(),
                (Some(t), _) => t.replace('_', " "),
                _ => "you".to_string(),
            });
        }
        "UserPromptSubmit" | "PostToolUse" => sess.waiting = None,
        _ => {}
    }
    store::record_activity(sess, now, days);
    if ev.hook_event_name == "SessionEnd" {
        sess.ended = true;
    }
    let cwd = if ev.cwd.is_empty() { sess.cwd.clone() } else { ev.cwd.clone() };

    if sess.item_id.is_none() {
        claim(state, &ev.session_id, &cwd, now, days, git);
    }

    if ev.hook_event_name == "PostToolUse" {
        post_tool_use(state, ev, &cwd, now, days, git);
    }

    let item = state.sessions.get(&ev.session_id).and_then(|s| s.item_id.clone());
    if let Some(i) = &item {
        if let Some(w) = state.items.get_mut(i) {
            w.last_activity_ms = w.last_activity_ms.max(now);
        }
    }
    let main = match (ev.hook_event_name.as_str(), item) {
        ("SessionStart", Some(i)) => Some(assigned_text(state, &ev.session_id, &i)),
        ("SessionStart" | "UserPromptSubmit", None) => Some(ask_text(state, &ev.session_id, now, window_days)),
        _ => None,
    };
    let switch = (ev.hook_event_name == "SessionStart").then(|| switch_text(state, &ev.session_id)).flatten();
    match (main, switch) {
        (Some(m), Some(s)) => Some(format!("{m}\n\n{s}")),
        (m, s) => m.or(s),
    }
}

/// A session with no work item takes one if the card's launch is waiting
/// for it, or if its branch names an issue some card already carries.
fn claim(state: &mut State, session_id: &str, cwd: &str, now: i64, days: &dyn Days, git: &dyn Git) {
    let launch = state
        .pending_launches
        .iter()
        .position(|p| p.expires_ms > now && (store::is_ignored(cwd, std::slice::from_ref(&p.repo))));
    if let Some(pos) = launch {
        let p = state.pending_launches.remove(pos);
        state.assign_session(session_id, &p.item_id, now, days, git);
        return;
    }
    if let Some(key) = git.branch(cwd).and_then(|b| store::issue_key_in(&b)) {
        if let Some(id) = state.item_by_key(&key).map(|i| i.id.clone()) {
            state.assign_session(session_id, &id, now, days, git);
        }
    }
}

fn post_tool_use(state: &mut State, ev: &HookEvent, cwd: &str, now: i64, days: &dyn Days, git: &dyn Git) {
    let tool = ev.tool_name.as_deref().unwrap_or("");
    if tool == "Bash" && is_git_commit(&ev.tool_input) {
        let Some((hash, subject)) = git.head(cwd) else { return };
        let sess = state.sessions.get_mut(&ev.session_id).expect("session exists");
        if sess.commits.contains(&hash) {
            return;
        }
        sess.commits.push(hash);
        let excess = sess.commits.len().saturating_sub(100);
        sess.commits.drain(..excess);
        match sess.item_id.clone() {
            Some(item) => state.add_line(&item, &days.date(now), &subject, "commit", now),
            None => sess.held_lines.push(Line { ms: now, text: subject, source: "commit".into() }),
        }
        return;
    }
    if let Some(bt) = bench_tool(tool) {
        let bench = BenchEvent { ms: now, tool: bt.to_string(), ok: !tool_failed(&ev.tool_response) };
        let sess = state.sessions.get_mut(&ev.session_id).expect("session exists");
        match sess.item_id.clone() {
            Some(item) => state.add_bench(&item, bench),
            None => sess.held_bench.push(bench),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::{FakeGit, UtcDays};
    use crate::store::PendingLaunch;

    const D0: i64 = 1_792_627_200_000;
    const H: i64 = 3_600_000;

    fn ev(name: &str, sid: &str, cwd: &str) -> HookEvent {
        HookEvent {
            hook_event_name: name.into(),
            session_id: sid.into(),
            cwd: cwd.into(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
            last_assistant_message: None,
            notification_type: None,
            message: None,
        }
    }

    #[test]
    fn the_last_reply_and_waiting_state_follow_the_hooks() {
        let mut st = State::default();
        let git = FakeGit::default();
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7);
        let mut n = ev("Notification", "s", "/w/r");
        n.notification_type = Some("permission_prompt".into());
        apply(&mut st, &n, D0 + 1, &UtcDays, &git, 7);
        assert_eq!(st.sessions["s"].waiting.as_deref(), Some("permission prompt"));
        let mut stop = ev("Stop", "s", "/w/r");
        stop.last_assistant_message = Some("Summary: the loader needs 432 KB.".into());
        apply(&mut st, &stop, D0 + 2, &UtcDays, &git, 7);
        assert_eq!(st.sessions["s"].waiting, None);
        assert_eq!(st.sessions["s"].last_reply.as_deref(), Some("Summary: the loader needs 432 KB."));
    }

    #[test]
    fn an_ignored_repo_gets_no_time_and_no_question() {
        let mut st = State { ignore: vec!["/g/embarch".into()], ..Default::default() };
        let out = apply(&mut st, &ev("SessionStart", "s", "/g/embarch/embarch-ui"), D0, &UtcDays, &FakeGit::default(), 7);
        assert!(out.is_none());
        assert!(st.sessions["s"].ignored);
        assert!(st.sessions["s"].spans.is_empty());
    }

    #[test]
    fn an_unassigned_session_is_asked_on_start_and_on_every_prompt() {
        let mut st = State::default();
        let git = FakeGit::default();
        let a = apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7).unwrap();
        assert!(a.contains("desk_set_work_item") && a.contains("`s`"));
        assert!(apply(&mut st, &ev("UserPromptSubmit", "s", "/w/r"), D0 + 1, &UtcDays, &git, 7).is_some());
        assert!(apply(&mut st, &ev("UserPromptSubmit", "s", "/w/r"), D0 + 2, &UtcDays, &git, 7).is_some());
        assert!(apply(&mut st, &ev("Stop", "s", "/w/r"), D0 + 3, &UtcDays, &git, 7).is_none());
    }

    #[test]
    fn a_card_launch_is_claimed_by_the_next_session_in_its_repo() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        st.pending_launches.push(PendingLaunch { item_id: w.clone(), repo: "/w/r".into(), expires_ms: D0 + H });
        let out = apply(&mut st, &ev("SessionStart", "s", "/w/r/app"), D0, &UtcDays, &FakeGit::default(), 7).unwrap();
        assert_eq!(st.sessions["s"].item_id.as_deref(), Some(w.as_str()));
        assert!(out.contains("desk_add_line"));
        assert!(st.pending_launches.is_empty());
    }

    #[test]
    fn an_expired_launch_is_not_claimed() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        st.pending_launches.push(PendingLaunch { item_id: w, repo: "/w/r".into(), expires_ms: D0 - 1 });
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &FakeGit::default(), 7);
        assert!(st.sessions["s"].item_id.is_none());
    }

    #[test]
    fn a_branch_naming_a_card_issue_claims_it() {
        let mut st = State::default();
        let issue = crate::store::IssueRef { key: "ABC-123".into(), ..Default::default() };
        let w = st.create_item("Bootload", Some(issue), D0);
        let git = FakeGit { branch: Some("gabriel/abc-123-bootload".into()), ..Default::default() };
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7);
        assert_eq!(st.sessions["s"].item_id.as_deref(), Some(w.as_str()));
    }

    #[test]
    fn a_commit_becomes_one_line_once() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        st.pending_launches.push(PendingLaunch { item_id: w.clone(), repo: "/w/r".into(), expires_ms: D0 + H });
        let git = FakeGit::default();
        *git.head.borrow_mut() = Some(("abc".into(), "smp: retry a busy upload".into()));
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7);
        let mut e = ev("PostToolUse", "s", "/w/r");
        e.tool_name = Some("Bash".into());
        e.tool_input = Some(serde_json::json!({"command": "git commit -m 'smp: retry a busy upload'"}));
        apply(&mut st, &e, D0 + 1, &UtcDays, &git, 7);
        apply(&mut st, &e, D0 + 2, &UtcDays, &git, 7);
        let lines = &st.tiles[&format!("{w}@2026-10-22")].lines;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].source, "commit");
    }

    #[test]
    fn a_session_in_another_desks_repo_is_asked_once_to_switch_and_keeps_its_desk() {
        let mut st = State::default();
        let a = st.put_desk(crate::store::Desk { name: "Alpha".into(), firmware_repos: vec!["/w/a".into()], ..Default::default() }, D0);
        let b = st.put_desk(crate::store::Desk { name: "Beta".into(), firmware_repos: vec!["/w/b".into()], ..Default::default() }, D0);
        st.open_desk = Some(a);
        let git = FakeGit::default();
        let out = apply(&mut st, &ev("SessionStart", "s", "/w/b/app"), D0, &UtcDays, &git, 7).unwrap();
        assert!(out.contains("\"Beta\"") && out.contains("desk_open"), "{out}");
        assert_eq!(st.sessions["s"].desk_id.as_deref(), Some(b.as_str()));
        let again = apply(&mut st, &ev("UserPromptSubmit", "s", "/w/b/app"), D0 + 1, &UtcDays, &git, 7).unwrap();
        assert!(!again.contains("desk_open"), "the switch question is asked once, at start");
        st.open_desk = Some(b);
        let in_open = apply(&mut st, &ev("SessionStart", "t", "/w/b"), D0 + 2, &UtcDays, &git, 7).unwrap();
        assert!(!in_open.contains("desk_open"));
    }

    #[test]
    fn embarch_api_flashes_are_bench_activity_and_reads_are_not() {
        assert_eq!(bench_tool("mcp__embarch-api__flash"), Some("flash"));
        assert_eq!(bench_tool("mcp__embarch-api__list_projects"), None);
        assert_eq!(bench_tool("mcp__harvest__log_time"), None);
        let mut st = State::default();
        let git = FakeGit::default();
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7);
        let mut e = ev("PostToolUse", "s", "/w/r");
        e.tool_name = Some("mcp__embarch-api__run_study".into());
        e.tool_response = Some(serde_json::json!({"isError": true}));
        apply(&mut st, &e, D0 + 1, &UtcDays, &git, 7);
        assert_eq!(st.sessions["s"].held_bench, vec![BenchEvent { ms: D0 + 1, tool: "run_study".into(), ok: false }]);
    }
}
