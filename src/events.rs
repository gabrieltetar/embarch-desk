//! What a Claude Code hook event does to the desk.
//!
//! The plugin's hooks forward their stdin JSON here verbatim. Every event of
//! a tracked session extends its time; a few do more:
//!
//! - **SessionStart** claims a pending "open in VS Code" launch, or a work
//!   item whose issue key the branch carries, and tells the session its desk
//!   id — the tools need it, and nothing else identifies a session to them —
//!   and its desk's standing notes (decision 74).
//! - **UserPromptSubmit** on a session with no work item asks, every prompt,
//!   until it is answered, an issue key in its opening prompt or a handoff taken first (proposal decision 7), unless time is off (decision 72).
//! - **PostToolUse** turns a `git commit` into a tile line, an
//!   `embarch-api` build, flash or study into bench activity, and a write
//!   to a Claude Code memory file into the item's memory note.
//!
//! A session under an ignored path is dropped at the door: no time, no
//! question, no context. One the user untracked moves only its state, and
//! hears only its desk's notes at its start.

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
    // The session's own desk's cards, the ones it is most likely to be; on
    // no desk, every desk's. Never the UI's (EMB-94).
    let scope = state.sessions.get(session_id).and_then(|s| s.desk_id.clone());
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
         its time has nowhere to land. First look in what you already have: an issue key in the \
         user's opening prompt or a handoff, of a team the desk's workspaces hold (`linear_teams` \
         lists the keys; a lookalike in prose is not one). If there is one, call the \
         `desk_set_work_item` tool with session_id `{session_id}` and that key, say in one line \
         which item you chose, and do not ask. If there is none, ask the user in one line which \
         work item it is — {choices} — or a Linear issue key, or a title for a new standalone \
         item, then call the tool with the answer (a new standalone item also needs its desk — \
         ask which). If the user says this is not tracked work, call it with untracked: true. \
         With no key anywhere, ask again on every prompt until it is answered."
    )
}

/// What a session on no work item hears on a desk with time off: the id its
/// tools need, and no question.
fn unasked_text(session_id: &str) -> String {
    format!(
        "EmbArch Desk: this session's desk session id is `{session_id}`. It is on no work item, and \
         that is fine: do not ask the user for one. If the user names an issue or an item, call the \
         `desk_set_work_item` tool with this session_id."
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
         {label}. The desk holds everything about it — the issue and its comments, PRs, \
         sessions, notes taken about the work, bench activity: call `desk_work_item` for it \
         when you need the item's context. As you work, add short notes with `desk_add_line` \
         — kind done for work finished without a commit, finding, decision, handoff or todo — \
         too many rather than too few. Commits and memory files are added automatically."
    )
}

/// A desk's standing notes as a session hears them (decision 74), or none
/// when it has none.
pub fn desk_notes_text(state: &State, desk: &str) -> Option<String> {
    let notes = state.desk_notes_of(desk);
    if notes.is_empty() {
        return None;
    }
    let name = state.desks.get(desk).map_or(desk, |d| d.name.as_str());
    Some(format!(
        "EmbArch Desk: standing notes for the desk \"{name}\", which hold for every session in its repos and are \
         kept on the Desk tab. Follow them; if one conflicts with what the user asks now, ask. Do not copy them into \
         memory files or work-item notes.\n{}",
        store::desk_note_list(notes)
    ))
}

/// A starting session's desk notes: those of the desk it belongs to now,
/// resolved at this start rather than read from its first event, and
/// recorded as the desk it was told of.
fn start_notes(state: &mut State, session_id: &str, cwd: &str) -> Option<String> {
    let desk = state.session_desk(session_id, cwd);
    if let Some(s) = state.sessions.get_mut(session_id) {
        s.start_desk = desk.clone();
    }
    desk_notes_text(state, &desk?)
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
    let time_on = state.settings.time;
    let desk_here = state.desk_for_path(&ev.cwd);
    let sess = state.sessions.entry(ev.session_id.clone()).or_insert_with(|| {
        let mut s = Session::new(&ev.session_id, &ev.cwd, now);
        s.desk_id = desk_here.clone();
        s.ignored = store::is_ignored(&ev.cwd, &ignore);
        s
    });
    // A session the panel put on its item before starting it (chat proposal
    // decision 10) learns its folder from its first event.
    if sess.cwd.is_empty() && !ev.cwd.is_empty() {
        sess.cwd = ev.cwd.clone();
        sess.desk_id = sess.desk_id.clone().or(desk_here);
        let item = sess.item_id.clone();
        if let Some(w) = item.and_then(|i| state.items.get_mut(&i)).filter(|w| w.repo_path.is_none()) {
            w.repo_path = git.toplevel(&ev.cwd);
        }
    }
    let sess = state.sessions.get_mut(&ev.session_id)?;
    if sess.ignored && store::is_ignored(&sess.cwd, &ignore) {
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
            sess.busy = false;
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
        "UserPromptSubmit" | "PostToolUse" => {
            sess.waiting = None;
            sess.busy = true;
        }
        "SessionStart" => {
            sess.waiting = None;
            sess.busy = false;
        }
        _ => {}
    }
    // A resumed session lives again: the panel closes an idle chat's process
    // and resumes it on the next message (chat proposal decision 41), and a
    // resume arrives as SessionStart. That one hook can be lost — the desk
    // restarting, the hook timing out — so the resume's first prompt counts
    // too: a prompt runs before its turn, and SessionEnd waits for the turn
    // to finish, so a prompt cannot trail one. Any other event after
    // SessionEnd — a Stop, a Notification — is a hook delivered late, and
    // leaves it ended.
    match ev.hook_event_name.as_str() {
        "SessionEnd" => sess.ended = true,
        "SessionStart" | "UserPromptSubmit" => sess.ended = false,
        _ => {}
    }
    if sess.ended {
        sess.busy = false;
    }
    let cwd = if ev.cwd.is_empty() { sess.cwd.clone() } else { ev.cwd.clone() };
    let start = ev.hook_event_name == "SessionStart";
    // One the user untracked still shows its state in the panel's thread
    // list (chat proposal decisions 9, 20); it makes no time and is never
    // asked, but still hears its desk's standing notes (decision 74). A
    // hidden one hears nothing.
    if sess.ignored {
        sess.last_ms = sess.last_ms.max(now);
        return if start { start_notes(state, &ev.session_id, &cwd) } else { None };
    }
    if time_on {
        store::record_activity(sess, now, days);
    }

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
        // Asking exists so time has somewhere to land (decision 72).
        ("SessionStart" | "UserPromptSubmit", None) if time_on => Some(ask_text(state, &ev.session_id, now, window_days)),
        ("SessionStart", None) => Some(unasked_text(&ev.session_id)),
        _ => None,
    };
    let notes = if start { start_notes(state, &ev.session_id, &cwd) } else { None };
    let parts: Vec<String> = [main, notes].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
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
            None => sess.held_lines.push(Line::new(now, &subject, "commit")),
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

/// The memory file a PostToolUse `Write`, `Edit` or `MultiEdit` just wrote,
/// if it wrote one (`embarch-chat-desk-proposal.md` 45). The server reads
/// its text and hands it to [`capture_memory`].
pub fn memory_write(ev: &HookEvent, claude_dir: &std::path::Path) -> Option<String> {
    if ev.hook_event_name != "PostToolUse" || !matches!(ev.tool_name.as_deref(), Some("Write" | "Edit" | "MultiEdit")) {
        return None;
    }
    let path = ev.tool_input.as_ref()?.get("file_path")?.as_str()?;
    crate::memory::is_memory_file(path, claude_dir).then(|| path.to_string())
}

/// A memory file's text after a write becomes the newest version of its
/// note on the session's work item — the session decides the item, not the
/// file's name — held until the session has one, as lines are.
pub fn capture_memory(state: &mut State, session_id: &str, path: &str, text: &str, now: i64, days: &dyn Days) {
    let Some(sess) = state.sessions.get_mut(session_id).filter(|s| !s.ignored) else { return };
    let mut note = Line::memory(now, path, text);
    note.session_id = Some(session_id.to_string());
    match sess.item_id.clone() {
        Some(item) => {
            state.add_note(&item, &days.date(now), note);
        }
        // Held, only the file's newest state matters: it lands as one
        // version.
        None => {
            sess.held_lines.retain(|l| !l.is_memory() || l.source_path != note.source_path);
            sess.held_lines.push(note);
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
        assert!(a.contains("opening prompt or a handoff") && a.contains("do not ask"), "{a}");
        assert!(a.contains("ask the user in one line") && a.contains("ask again on every prompt"), "{a}");
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
        assert!(out.contains("desk_add_line") && out.contains("desk_work_item"), "{out}");
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
    fn a_session_in_a_desk_the_ui_does_not_show_works_on_its_own_and_is_asked_nothing_about_it() {
        let mut st = State::default();
        let a = st.put_desk(crate::store::Desk { name: "Alpha".into(), firmware_repos: vec!["/w/a".into()], ..Default::default() }, D0);
        let b = st.put_desk(crate::store::Desk { name: "Beta".into(), firmware_repos: vec!["/w/b".into()], ..Default::default() }, D0);
        let w = st.create_item("on alpha", None, D0);
        st.items.get_mut(&w).unwrap().desk_id = Some(a.clone());
        st.ui_desk = Some(a.clone());
        let git = FakeGit::default();
        let out = apply(&mut st, &ev("SessionStart", "s", "/w/b/app"), D0, &UtcDays, &git, 7).unwrap();
        assert!(!out.contains("desk_open") && !out.contains("switch"), "{out}");
        assert!(!out.contains("on alpha"), "it is offered its own desk's cards, not the UI's: {out}");
        assert_eq!(st.sessions["s"].desk_id.as_deref(), Some(b.as_str()));
        assert_eq!(st.caller_desk(Some("s"), Some("/w/b/app")).as_deref(), Some(b.as_str()));
        st.ui_desk = None;
        assert_eq!(st.caller_desk(Some("s"), Some("/w/b/app")).as_deref(), Some(b.as_str()), "the UI moves nothing for it");
    }

    fn write(sid: &str, tool: &str, path: &str) -> HookEvent {
        let mut e = ev("PostToolUse", sid, "/w/r");
        e.tool_name = Some(tool.into());
        e.tool_input = Some(serde_json::json!({ "file_path": path }));
        e
    }

    #[test]
    fn a_memory_write_becomes_the_sessions_items_note_held_until_it_has_one() {
        let claude = std::path::Path::new("/h/.claude");
        let file = "/h/.claude/projects/-w-r/memory/loader-state.md";
        for tool in ["Write", "Edit", "MultiEdit"] {
            assert_eq!(memory_write(&write("s", tool, file), claude).as_deref(), Some(file));
        }
        assert!(memory_write(&write("s", "Read", file), claude).is_none());
        assert!(memory_write(&write("s", "Write", "/h/.claude/projects/-w-r/memory/MEMORY.md"), claude).is_none());
        assert!(memory_write(&write("s", "Write", "/w/r/memory/x.md"), claude).is_none());

        let mut st = State::default();
        let git = FakeGit::default();
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7);
        capture_memory(&mut st, "s", file, "branch user/abc-1", D0 + 1, &UtcDays);
        assert_eq!(st.sessions["s"].held_lines[0].kind, "memory");
        // The session decides the item, not the file's name.
        let w = st.create_item("Loader", None, D0);
        st.assign_session("s", &w, D0 + 2, &UtcDays, &git);
        capture_memory(&mut st, "s", file, "branch user/abc-1; PR open", D0 + 3, &UtcDays);
        let notes = &st.notes[&w];
        assert_eq!(notes.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["branch user/abc-1", "branch user/abc-1; PR open"]);
        assert!(notes.iter().all(|l| l.session_id.as_deref() == Some("s") && l.source_path.as_deref() == Some(file) && !l.id.is_empty()));
        assert!(st.tiles.is_empty(), "a memory file is not a tile line");

        let mut ignored = State { ignore: vec!["/g/embarch".into()], ..Default::default() };
        apply(&mut ignored, &ev("SessionStart", "f", "/g/embarch/x"), D0, &UtcDays, &git, 7);
        capture_memory(&mut ignored, "f", file, "fleet", D0 + 1, &UtcDays);
        assert!(ignored.sessions["f"].held_lines.is_empty());
    }

    #[test]
    fn a_session_the_panel_put_on_its_item_learns_its_folder_and_a_resumed_one_lives_again() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        let mut pre = Session::new("c", "", D0);
        pre.client = Some("embarch-chat".into());
        st.sessions.insert("c".into(), pre);
        st.assign_session("c", &w, D0, &UtcDays, &FakeGit { toplevel: Some("/elsewhere".into()), ..Default::default() });
        assert_eq!(st.items[&w].repo_path, None, "no folder yet, so no repo guessed from the desk's own");
        let git = FakeGit { toplevel: Some("/w/fw-abc-1".into()), ..Default::default() };
        let out = apply(&mut st, &ev("SessionStart", "c", "/w/fw-abc-1"), D0 + 1, &UtcDays, &git, 7).unwrap();
        assert!(out.contains(&w));
        assert_eq!(st.sessions["c"].cwd, "/w/fw-abc-1");
        assert_eq!(st.items[&w].repo_path.as_deref(), Some("/w/fw-abc-1"));
        apply(&mut st, &ev("UserPromptSubmit", "c", "/w/fw-abc-1"), D0 + 2, &UtcDays, &git, 7);
        assert!(st.sessions["c"].busy);
        apply(&mut st, &ev("Stop", "c", "/w/fw-abc-1"), D0 + 3, &UtcDays, &git, 7);
        assert!(!st.sessions["c"].busy);
        apply(&mut st, &ev("SessionEnd", "c", "/w/fw-abc-1"), D0 + 4, &UtcDays, &git, 7);
        assert!(st.sessions["c"].ended);
        apply(&mut st, &ev("SessionStart", "c", "/w/fw-abc-1"), D0 + 5, &UtcDays, &git, 7);
        assert!(!st.sessions["c"].ended, "the panel resumed it");
        assert_eq!(st.sessions["c"].client.as_deref(), Some("embarch-chat"));
    }

    #[test]
    fn a_memory_file_held_for_a_session_with_no_item_keeps_only_its_newest_state() {
        let file = "/h/.claude/projects/-w-r/memory/loader-state.md";
        let other = "/h/.claude/projects/-w-r/memory/bring-up.md";
        let mut st = State::default();
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &FakeGit::default(), 7);
        for i in 0..50 {
            capture_memory(&mut st, "s", file, &format!("state {i}"), D0 + i, &UtcDays);
        }
        capture_memory(&mut st, "s", other, "bring-up", D0 + 60, &UtcDays);
        let held: Vec<&str> = st.sessions["s"].held_lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(held, ["state 49", "bring-up"]);
    }

    #[test]
    fn a_hook_delivered_after_session_end_leaves_the_session_ended() {
        let mut st = State::default();
        let git = FakeGit::default();
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7);
        apply(&mut st, &ev("SessionEnd", "s", "/w/r"), D0 + 1, &UtcDays, &git, 7);
        for late in ["Stop", "Notification", "PostToolUse"] {
            apply(&mut st, &ev(late, "s", "/w/r"), D0 + 2, &UtcDays, &git, 7);
            assert!(st.sessions["s"].ended, "{late} after SessionEnd");
        }
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0 + 3, &UtcDays, &git, 7);
        assert!(!st.sessions["s"].ended, "a resume arrives as SessionStart");
    }

    #[test]
    fn a_resume_whose_session_start_was_lost_lives_again_on_its_first_prompt() {
        let mut st = State::default();
        let git = FakeGit::default();
        let w = st.create_item("x", None, D0);
        apply(&mut st, &ev("SessionStart", "s", "/w/r"), D0, &UtcDays, &git, 7);
        st.assign_session("s", &w, D0, &UtcDays, &git);
        apply(&mut st, &ev("SessionEnd", "s", "/w/r"), D0 + H, &UtcDays, &git, 7);
        // Resumed while the desk restarted: its SessionStart never arrived.
        apply(&mut st, &ev("UserPromptSubmit", "s", "/w/r"), D0 + 2 * H, &UtcDays, &git, 7);
        assert!(!st.sessions["s"].ended && st.sessions["s"].busy, "a prompt runs before its turn, so it cannot trail a SessionEnd");
        apply(&mut st, &ev("Stop", "s", "/w/r"), D0 + 2 * H + 1, &UtcDays, &git, 7);
        apply(&mut st, &ev("SessionEnd", "s", "/w/r"), D0 + 2 * H + 2, &UtcDays, &git, 7);
        apply(&mut st, &ev("Stop", "s", "/w/r"), D0 + 2 * H + 3, &UtcDays, &git, 7);
        assert!(st.sessions["s"].ended, "a late Stop still leaves it ended");
    }

    #[test]
    fn an_untracked_sessions_hooks_move_its_state_but_make_no_time() {
        let mut st = State { ignore: vec!["/g/embarch".into()], ..Default::default() };
        let git = FakeGit::default();
        apply(&mut st, &ev("SessionStart", "u", "/w/r"), D0, &UtcDays, &git, 7);
        st.untrack_session("u");
        assert!(apply(&mut st, &ev("UserPromptSubmit", "u", "/w/r"), D0 + H, &UtcDays, &git, 7).is_none(), "never asked");
        let s = &st.sessions["u"];
        assert!(s.busy && s.last_ms == D0 + H && s.spans.is_empty());
        let mut n = ev("Notification", "u", "/w/r");
        n.notification_type = Some("permission_prompt".into());
        apply(&mut st, &n, D0 + H + 1, &UtcDays, &git, 7);
        assert_eq!(st.sessions["u"].waiting.as_deref(), Some("permission prompt"));
        apply(&mut st, &ev("SessionEnd", "u", "/w/r"), D0 + H + 2, &UtcDays, &git, 7);
        assert!(st.sessions["u"].ended && st.sessions["u"].spans.is_empty());
        // The fleet's sessions still stop at the door.
        apply(&mut st, &ev("SessionStart", "f", "/g/embarch/x"), D0, &UtcDays, &git, 7);
        apply(&mut st, &ev("UserPromptSubmit", "f", "/g/embarch/x"), D0 + H, &UtcDays, &git, 7);
        assert!(!st.sessions["f"].busy && st.sessions["f"].last_ms == D0);
    }

    #[test]
    fn a_desk_with_time_off_keeps_session_state_but_makes_no_spans() {
        let mut st = State::default();
        st.settings.time = false;
        let git = FakeGit::default();
        apply(&mut st, &ev("SessionStart", "d", "/w/r"), D0, &UtcDays, &git, 7);
        apply(&mut st, &ev("UserPromptSubmit", "d", "/w/r"), D0 + H, &UtcDays, &git, 7);
        let s = &st.sessions["d"];
        assert!(s.busy && s.spans.is_empty(), "state moves, no time is made");
    }

    #[test]
    fn a_desk_with_time_off_never_asks_and_still_tells_a_session_its_item() {
        let mut st = State::default();
        st.settings.time = false;
        let git = FakeGit::default();
        let start = apply(&mut st, &ev("SessionStart", "q", "/w/r"), D0, &UtcDays, &git, 7).unwrap();
        assert!(start.contains("`q`") && start.contains("do not ask") && !start.contains("ask again"), "{start}");
        assert!(apply(&mut st, &ev("UserPromptSubmit", "q", "/w/r"), D0, &UtcDays, &git, 7).is_none(), "a prompt hears nothing");
        assert!(st.sessions["q"].item_id.is_none() && !st.sessions["q"].ignored, "it stays listed, with no item");
        let w = st.create_item("x", None, D0);
        st.assign_session("q", &w, D0, &UtcDays, &git);
        let told = apply(&mut st, &ev("SessionStart", "q", "/w/r"), D0 + H, &UtcDays, &git, 7).unwrap();
        assert!(told.contains(&w), "{told}");
    }

    fn desk_with_notes(st: &mut State) -> String {
        let k = st.put_desk(crate::store::Desk { name: "Alpha".into(), firmware_repos: vec!["/w/x".into()], ..Default::default() }, D0);
        st.add_desk_note(&k, "in repo X, only app Y on board Z matters", "user", None, D0).unwrap();
        st.add_desk_note(&k, "file F is another engineer's:\nreport findings, never open a fix branch", "agent", Some("s0"), D0).unwrap();
        k
    }

    const NOTES_HEAD: &str = "EmbArch Desk: standing notes for the desk \"Alpha\"";

    #[test]
    fn a_session_starting_in_the_desks_folder_hears_its_notes_and_a_prompt_does_not() {
        let mut st = State::default();
        desk_with_notes(&mut st);
        let git = FakeGit::default();
        let out = apply(&mut st, &ev("SessionStart", "s", "/w/x/app"), D0, &UtcDays, &git, 7).unwrap();
        let (ask, notes) = out.split_once("\n\nEmbArch Desk: standing notes").unwrap_or_else(|| panic!("{out}"));
        assert!(ask.contains("desk_set_work_item"), "the notes come after what the session is told already");
        assert!(notes.ends_with(
            "for the desk \"Alpha\", which hold for every session in its repos and are kept on the Desk tab. Follow them; \
             if one conflicts with what the user asks now, ask. Do not copy them into memory files or work-item notes.\n\
             - in repo X, only app Y on board Z matters\n\
             - file F is another engineer's:\n  report findings, never open a fix branch (added by an agent)"
        ), "{notes}");
        assert!(!out.contains("by the user"), "an agent wrote one of them");
        let prompt = apply(&mut st, &ev("UserPromptSubmit", "s", "/w/x/app"), D0 + 1, &UtcDays, &git, 7).unwrap();
        assert!(!prompt.contains(NOTES_HEAD), "SessionStart only: {prompt}");
        // A resume, a clear and a compact each arrive as a SessionStart too.
        let again = apply(&mut st, &ev("SessionStart", "s", "/w/x/app"), D0 + 2, &UtcDays, &git, 7).unwrap();
        assert!(again.contains(NOTES_HEAD), "{again}");
    }

    #[test]
    fn a_session_outside_the_desks_folders_hears_its_notes_through_its_work_item() {
        let mut st = State::default();
        let k = desk_with_notes(&mut st);
        let w = st.create_item("x", None, D0);
        st.items.get_mut(&w).unwrap().desk_id = Some(k);
        st.pending_launches.push(PendingLaunch { item_id: w.clone(), repo: "/w/other".into(), expires_ms: D0 + H });
        let out = apply(&mut st, &ev("SessionStart", "s", "/w/other"), D0, &UtcDays, &FakeGit::default(), 7).unwrap();
        assert_eq!(st.sessions["s"].desk_id, None, "its first event found no desk for its folder");
        assert!(out.contains(&w) && out.contains(NOTES_HEAD), "{out}");
    }

    #[test]
    fn an_untracked_session_hears_its_desks_notes_and_a_hidden_or_ignored_one_nothing() {
        let mut st = State { ignore: vec!["/w/x/vendor".into()], ..Default::default() };
        desk_with_notes(&mut st);
        let git = FakeGit::default();
        for sid in ["u", "h"] {
            apply(&mut st, &ev("SessionStart", sid, "/w/x"), D0, &UtcDays, &git, 7);
        }
        st.untrack_session("u");
        st.hide_session("h");
        let u = apply(&mut st, &ev("SessionStart", "u", "/w/x"), D0 + 1, &UtcDays, &git, 7).unwrap();
        assert!(u.starts_with(NOTES_HEAD) && !u.contains("desk_set_work_item"), "{u}");
        assert!(apply(&mut st, &ev("UserPromptSubmit", "u", "/w/x"), D0 + 2, &UtcDays, &git, 7).is_none());
        assert!(apply(&mut st, &ev("SessionStart", "h", "/w/x"), D0 + 1, &UtcDays, &git, 7).is_none());
        assert!(apply(&mut st, &ev("SessionStart", "i", "/w/x/vendor"), D0, &UtcDays, &git, 7).is_none());
    }

    #[test]
    fn one_start_names_one_desk_in_its_notes_and_keeps_it() {
        let mut st = State::default();
        let a = desk_with_notes(&mut st);
        let b = st.put_desk(crate::store::Desk { name: "Beta".into(), firmware_repos: vec!["/w/b".into()], ..Default::default() }, D0);
        st.add_desk_note(&b, "on board Q, flash over SWD only", "user", None, D0).unwrap();
        st.ui_desk = Some(a.clone());
        let git = FakeGit::default();
        apply(&mut st, &ev("SessionStart", "s", "/w/x"), D0, &UtcDays, &git, 7);
        assert_eq!(st.sessions["s"].start_desk.as_deref(), Some(a.as_str()));
        // Resumed from another desk's repo: its first folder is still /w/x.
        let out = apply(&mut st, &ev("SessionStart", "s", "/w/b/app"), D0 + H, &UtcDays, &git, 7).unwrap();
        assert!(out.contains("standing notes for the desk \"Beta\"") && !out.contains("belongs to the desk"), "{out}");
        assert!(out.contains("flash over SWD") && !out.contains("board Z"), "{out}");
        assert_eq!((st.sessions["s"].cwd.as_str(), st.sessions["s"].desk_id.as_deref()), ("/w/x", Some(a.as_str())));
        assert_eq!(st.notes_desk("s").as_deref(), Some(b.as_str()), "what it reads back and writes to is the desk it was told of");
    }

    #[test]
    fn the_notes_a_session_hears_are_never_longer_than_the_cap() {
        let mut st = State::default();
        let k = st.put_desk(crate::store::Desk { name: "Alpha".into(), firmware_repos: vec!["/w/x".into()], ..Default::default() }, D0);
        let full = vec!["x"; 125].join("\n");
        let mut agent = false;
        while st.add_desk_note(&k, &full, if agent { "agent" } else { "user" }, agent.then_some("s"), D0).is_ok() {
            agent = !agent;
        }
        let block = desk_notes_text(&st, &k).unwrap();
        let (head, list) = block.split_once('\n').unwrap();
        assert!(head.starts_with(NOTES_HEAD) && list.starts_with("- x\n  x"), "{block}");
        assert!(list.chars().count() <= crate::store::DESK_NOTES_MAX_CHARS, "{}", list.chars().count());
    }

    #[test]
    fn a_desk_with_no_notes_adds_nothing_to_the_start() {
        let mut st = State::default();
        let k = desk_with_notes(&mut st);
        let ids: Vec<String> = st.desk_notes_of(&k).iter().map(|n| n.id.clone()).collect();
        for id in ids {
            st.delete_desk_note(&k, &id).unwrap();
        }
        st.ui_desk = Some(k);
        let git = FakeGit::default();
        let out = apply(&mut st, &ev("SessionStart", "s", "/w/x"), D0, &UtcDays, &git, 7).unwrap();
        assert!(!out.contains("standing notes"), "{out}");
        let mut bare = State::default();
        let before = apply(&mut bare, &ev("SessionStart", "s", "/w/x"), D0, &UtcDays, &git, 7).unwrap();
        assert_eq!(out, before, "the start reads as it did before desk notes");
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
