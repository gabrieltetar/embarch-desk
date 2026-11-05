//! The pull (`embarch-chat-desk-proposal.md` 40): everything the desk holds
//! about a work item, as one text a session reads when it needs the item's
//! context — the issue with its latest comments, PRs, sessions, notes and
//! bench activity, each newest first, cut at [`BRIEF_MAX`] characters
//! (about 4K tokens). Nothing is pasted into a session up front; it calls
//! `desk_work_item` for this.
//!
//! The notes get what the rest leaves: each memory file's newest state and
//! the latest lines, a third of it kept for the lines so a large memory
//! file cannot crowd them out. What does not fit is counted, and
//! `GET /items/{id}/notes` has every note.

use crate::server::{session_state, Cache};
use crate::store::{Days, Line, State};
use serde_json::{json, Value};

pub const BRIEF_MAX: usize = 16_000;
const DESCRIPTION_HEAD: usize = 1_500;
const COMMENTS: usize = 5;
const COMMENT_MAX: usize = 600;
const SESSIONS: usize = 8;
const BENCH: usize = 15;
const LINE_MAX: usize = 400;

/// At most `n` characters, `…` marking a cut.
pub fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn flat(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn len(s: &str) -> usize {
    s.chars().count()
}

/// A note as the routes hand it out: `{id, ts_ms, session_id, kind, text,
/// source_path}`, plus its source and day, and whether it is its memory
/// file's newest version (a card shows each note's newest state, 46).
pub fn note_json(l: &Line, latest: bool, days: &dyn Days) -> Value {
    json!({
        "id": l.id,
        "ts_ms": l.ms,
        "session_id": l.session_id,
        "kind": l.kind,
        "text": l.text,
        "source_path": l.source_path,
        "source": l.source,
        "date": days.date(l.ms),
        "latest": latest,
    })
}

fn line_text(l: &Line, days: &dyn Days) -> String {
    let tag = if l.kind == "done" { l.source.clone() } else { l.kind.clone() };
    format!("- {} [{tag}] {}\n", days.stamp(l.ms), cut(&flat(&l.text), LINE_MAX))
}

/// The brief for a work item; `None` when there is no such item. `issue` is
/// the issue as Linear answers it now (`linear::read_issue`), or `None` with
/// the reason when it could not be read.
pub fn render(st: &State, cache: &Cache, item_id: &str, issue: Option<&Value>, issue_error: Option<&str>, days: &dyn Days) -> Option<String> {
    let item = st.items.get(item_id)?;

    let mut head = match &item.issue {
        Some(r) => {
            let state = issue.and_then(|i| i["state"].as_str()).unwrap_or(&r.state_name);
            format!("Work item {}: {} {} — {} ({})\n", item.id, r.key, item.title, state, r.url)
        }
        None => format!("Work item {}: {} (no Linear issue)\n", item.id, item.title),
    };
    let desk = item.desk_id.as_ref().and_then(|k| st.desks.get(k)).map(|d| d.name.clone());
    let mut facts = vec![];
    if let Some(d) = desk {
        facts.push(format!("desk {d}"));
    }
    if let Some(r) = &item.repo_path {
        facts.push(format!("repo {r}"));
    }
    if !facts.is_empty() {
        head.push_str(&format!("{}\n", facts.join(" · ")));
    }

    let mut issue_s = String::new();
    if item.issue.is_some() {
        issue_s.push_str("\n## Issue\n");
        match issue {
            Some(i) => {
                let mut about = vec![];
                for (label, key) in [("team", "team"), ("project", "project"), ("assignee", "assignee"), ("priority", "priority")] {
                    if let Some(v) = i[key].as_str().filter(|v| !v.is_empty()) {
                        about.push(format!("{label} {v}"));
                    }
                }
                let labels: Vec<&str> = i["labels"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if !labels.is_empty() {
                    about.push(format!("labels {}", labels.join(", ")));
                }
                if !about.is_empty() {
                    issue_s.push_str(&format!("{}\n", about.join(" · ")));
                }
                match i["description"].as_str().map(str::trim).filter(|d| !d.is_empty()) {
                    Some(d) => issue_s.push_str(&format!("\n{}\n", cut(d, DESCRIPTION_HEAD))),
                    None => issue_s.push_str("\n(no description)\n"),
                }
                // read_issue lists the latest comments oldest first.
                let comments: Vec<&Value> = i["comments"].as_array().into_iter().flatten().rev().take(COMMENTS).collect();
                if !comments.is_empty() {
                    issue_s.push_str("\nLatest comments, newest first:\n");
                    for c in comments {
                        let at = c["at"].as_str().unwrap_or("").replace('T', " ");
                        issue_s.push_str(&format!(
                            "- {} {}: {}\n",
                            at.get(..16).unwrap_or(&at),
                            c["author"].as_str().unwrap_or("someone"),
                            cut(&flat(c["body"].as_str().unwrap_or("")), COMMENT_MAX)
                        ));
                    }
                }
            }
            None => issue_s.push_str(&format!(
                "(Linear could not be read{}; linear_get_issue has the description and comments)\n",
                issue_error.map(|e| format!(": {}", cut(e, 200))).unwrap_or_default()
            )),
        }
    }

    let mut prs = String::from("\n## PRs\n");
    if item.prs.is_empty() {
        prs.push_str("- none found\n");
    }
    for p in item.prs.iter().take(10) {
        let draft = if p.draft { ", draft" } else { "" };
        prs.push_str(&format!("- {}#{} ({}{draft}): {} {}\n", p.repo, p.number, p.state, p.title, p.url));
    }

    let mut sessions: Vec<&crate::store::Session> = st
        .sessions
        .values()
        .filter(|s| !s.ignored && !State::is_timer_session(&s.id) && s.item_id.as_deref() == Some(item_id))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_ms));
    let mut sess = String::from("\n## Sessions, newest first\n");
    if sessions.is_empty() {
        sess.push_str("- none yet\n");
    }
    for s in sessions.iter().take(SESSIONS) {
        let (state, waiting) = session_state(cache, s);
        let waiting = waiting.map(|w| format!(", waiting for {w}")).unwrap_or_default();
        let client = s.client.as_deref().map(|c| format!(" · {c}")).unwrap_or_default();
        sess.push_str(&format!("- {} {state}{waiting}{client} · {} · {}\n", days.stamp(s.last_ms), s.cwd, s.id));
    }
    if sessions.len() > SESSIONS {
        sess.push_str(&format!("- … and {} earlier\n", sessions.len() - SESSIONS));
    }

    let mut bench = String::from("\n## Bench, newest first\n");
    if item.bench.is_empty() {
        bench.push_str("- none\n");
    }
    for b in item.bench.iter().rev().take(BENCH) {
        bench.push_str(&format!("- {} {} {}\n", days.stamp(b.ms), b.tool, if b.ok { "ok" } else { "FAILED" }));
    }

    // The notes take what is left.
    let fixed = len(&head) + len(&issue_s) + len(&prs) + len(&sess) + len(&bench);
    let notes_head = "\n## Notes, newest first\n";
    let budget = BRIEF_MAX.saturating_sub(fixed + len(notes_head) + 200);
    let all = st.item_notes(item_id);
    let memory: Vec<&Line> = st.memory_latest(item_id);
    let lines: Vec<String> = all.iter().filter(|l| !l.is_memory()).map(|l| line_text(l, days)).collect();
    let lines_total: usize = lines.iter().map(|l| len(l)).sum();
    let reserved = lines_total.min(budget / 3);
    let mut used = 0;
    let mut omitted = 0;
    let mut mem_s = String::new();
    for m in &memory {
        let name = m.source_path.as_deref().and_then(|p| p.rsplit('/').next()).unwrap_or("memory");
        let versions = all.iter().filter(|l| l.is_memory() && l.source_path == m.source_path).count();
        let title = format!("\n### Memory file {name} — {}, {versions} version{}\n", days.stamp(m.ms), if versions == 1 { "" } else { "s" });
        let room = budget.saturating_sub(reserved + used + len(&title) + 1);
        if room < 300 {
            omitted += 1;
            continue;
        }
        let block = format!("{title}{}\n", cut(m.text.trim(), room));
        used += len(&block);
        mem_s.push_str(&block);
    }
    let mut lines_s = String::new();
    let mut shown = 0;
    for l in &lines {
        if used + len(l) > budget {
            break;
        }
        used += len(l);
        lines_s.push_str(l);
        shown += 1;
    }
    omitted += lines.len() - shown;
    let mut notes = String::from(notes_head);
    if memory.is_empty() && lines.is_empty() {
        notes.push_str("- none yet\n");
    }
    notes.push_str(&mem_s);
    if !lines_s.is_empty() {
        notes.push_str(if mem_s.is_empty() { "" } else { "\n### Lines\n" });
        notes.push_str(&lines_s);
    }
    if omitted > 0 {
        notes.push_str(&format!("- … {omitted} older note{} not shown; the Desk card lists every note\n", if omitted == 1 { "" } else { "s" }));
    }

    let text = format!("{head}{issue_s}{prs}{sess}{notes}{bench}");
    Some(cut(&text, BRIEF_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::{session, UtcDays};
    use crate::store::{BenchEvent, IssueRef, PrRef};

    const D0: i64 = 1_792_627_200_000;
    const H: i64 = 3_600_000;

    fn item(st: &mut State) -> String {
        let issue = IssueRef { key: "ABC-123".into(), title: "Loader".into(), state_name: "In Progress".into(), url: "https://linear.app/x/issue/ABC-123".into(), ..Default::default() };
        let w = st.create_item("Loader", Some(issue), D0);
        st.items.get_mut(&w).unwrap().prs.push(PrRef { repo: "o/fw".into(), number: 7, title: "loader".into(), url: "https://github.com/o/fw/pull/7".into(), state: "open".into(), draft: false });
        w
    }

    #[test]
    fn the_brief_holds_the_issue_prs_sessions_notes_and_bench_newest_first() {
        let mut st = State::default();
        let w = item(&mut st);
        st.add_line(&w, "2026-10-22", "older line", "agent", D0 + H);
        let mut f = Line::new(D0 + 2 * H, "the loader needs 432 KB", "agent");
        f.kind = "finding".into();
        st.add_note(&w, "2026-10-22", f);
        st.add_note(&w, "2026-10-22", Line::memory(D0 + 3 * H, "/m/abc-123.md", "v1"));
        st.add_note(&w, "2026-10-22", Line::memory(D0 + 4 * H, "/m/abc-123.md", "branch user/abc-123; PR 7 open"));
        st.add_bench(&w, BenchEvent { ms: D0 + H, tool: "flash".into(), ok: false });
        st.add_bench(&w, BenchEvent { ms: D0 + 2 * H, tool: "run_study".into(), ok: true });
        let mut s = session("s1");
        s.item_id = Some(w.clone());
        s.client = Some("embarch-chat".into());
        s.busy = true;
        st.sessions.insert("s1".into(), s);
        let issue = json!({
            "state": "In Review", "team": "ABC", "description": "Make the loader fit.",
            "comments": [{"at": "2026-10-21T09:00:00.000Z", "author": "A", "body": "an early remark"}, {"at": "2026-10-22T09:00:00.000Z", "author": "B", "body": "a late remark"}],
        });
        let t = render(&st, &Cache::default(), &w, Some(&issue), None, &UtcDays).unwrap();
        assert!(t.starts_with(&format!("Work item {w}: ABC-123 Loader — In Review")), "{t}");
        assert!(t.find("2026-10-22 09:00 B: a late remark").unwrap() < t.find("A: an early remark").unwrap(), "comments newest first");
        assert!(t.contains("o/fw#7 (open)") && t.contains("busy · embarch-chat"));
        assert!(t.contains("Memory file abc-123.md — 2026-10-22 04:00, 2 versions\nbranch user/abc-123; PR 7 open"), "{t}");
        assert!(!t.contains("\nv1\n"), "only the memory file's newest state");
        assert!(t.find("[finding] the loader").unwrap() < t.find("[agent] older line").unwrap());
        assert!(t.find("run_study ok").unwrap() < t.find("flash FAILED").unwrap());
        let order: Vec<usize> = ["## Issue", "## PRs", "## Sessions", "## Notes", "## Bench"].iter().map(|h| t.find(h).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }

    #[test]
    fn the_brief_is_cut_at_its_cap_and_keeps_room_for_the_latest_lines() {
        let mut st = State::default();
        let w = item(&mut st);
        for i in 0..4 {
            st.add_note(&w, "2026-10-22", Line::memory(D0 + i * H, &format!("/m/f{i}.md"), &"x".repeat(9_000)));
        }
        for i in 0..200 {
            st.add_line(&w, "2026-10-22", &format!("line {i} {}", "y".repeat(100)), "agent", D0 + 5 * H + i);
        }
        let t = render(&st, &Cache::default(), &w, None, Some("offline"), &UtcDays).unwrap();
        assert!(len(&t) <= BRIEF_MAX, "{}", len(&t));
        assert!(t.contains("Linear could not be read: offline"));
        assert!(t.contains("line 199 "), "the newest line makes it in");
        assert!(t.contains("Memory file f3.md"), "the newest memory file makes it in");
        assert!(t.contains("older notes not shown"));
        assert!(t.contains("## Bench"), "the cut falls on the notes, not on what follows");
    }

    #[test]
    fn a_standalone_item_has_no_issue_section_and_an_unknown_one_no_brief() {
        let mut st = State::default();
        let w = st.create_item("Client call", None, D0);
        let t = render(&st, &Cache::default(), &w, None, None, &UtcDays).unwrap();
        assert!(t.starts_with(&format!("Work item {w}: Client call (no Linear issue)")));
        assert!(!t.contains("## Issue") && t.contains("- none yet"));
        assert!(render(&st, &Cache::default(), "w404", None, None, &UtcDays).is_none());
    }
}
