//! The desk's one state file and every rule about time.
//!
//! **The work item is the join** none of the four services holds: a Linear
//! issue (usually), its PRs, the Claude Code sessions that worked on it, its
//! bench activity and its time. **Time is session-open time** — a session's
//! first event to its last, idle gaps included — kept as one span per
//! (session, work item, local day), and a tile's seconds are the plain sum of
//! its spans: two sessions open on one item at once both count in full.
//! Over-capture is the rule; the tile is where it is cut.
//!
//! Everything here is plain data plus pure functions over it, so the rules
//! are tested without a clock, a git checkout or a network.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A gap longer than this ends a span instead of counting. Decision: idle
/// gaps count — a three-hour lunch is still session time — but a VS Code
/// window left open overnight is not a night of work.
pub const MAX_GAP_MS: i64 = 8 * 3600 * 1000;
/// How long a card's "open in VS Code" waits for its session to start.
pub const LAUNCH_CLAIM_MS: i64 = 10 * 60 * 1000;
/// Acted-on tiles older than this move to the monthly history file.
pub const ARCHIVE_AFTER_MS: i64 = 7 * 24 * 3600 * 1000;
/// Sessions quiet this long, with nothing left on an open tile, are dropped.
pub const SESSION_TTL_MS: i64 = 35 * 24 * 3600 * 1000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub items: BTreeMap<String, WorkItem>,
    #[serde(default)]
    pub sessions: BTreeMap<String, Session>,
    #[serde(default)]
    pub tiles: BTreeMap<String, Tile>,
    #[serde(default)]
    pub pending_launches: Vec<PendingLaunch>,
    /// Path prefixes never tracked: no tile, no question. EmbArch's own
    /// repos go here, which is also what keeps the agent fleet out.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// What agents asked to write to Linear, waiting for the user's Send.
    #[serde(default)]
    pub drafts: BTreeMap<String, Draft>,
    /// The one manual timer — work away from Claude Code (bench, calls,
    /// meetings). It runs as a session of its own, so its time is spans like
    /// any other and lands on the same tile.
    #[serde(default)]
    pub timer: Option<Timer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub issue: Option<IssueRef>,
    #[serde(default)]
    pub repo_path: Option<String>,
    #[serde(default)]
    pub prs: Vec<PrRef>,
    #[serde(default)]
    pub bench: Vec<BenchEvent>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    pub created_ms: i64,
    pub last_activity_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IssueRef {
    /// The Linear connector account label that answered for this key.
    pub account: String,
    pub key: String,
    pub id: String,
    pub url: String,
    pub title: String,
    pub state_id: String,
    pub state_name: String,
    pub state_type: String,
    pub team_id: String,
    pub refreshed_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrRef {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub draft: bool,
    /// `success`, `failure`, `pending` or `none`.
    pub ci: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchEvent {
    pub ms: i64,
    pub tool: String,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub ignored: bool,
    pub started_ms: i64,
    pub last_ms: i64,
    #[serde(default)]
    pub spans: Vec<Span>,
    /// Commit hashes already turned into lines, so a re-read HEAD is not a
    /// second line.
    #[serde(default)]
    pub commits: Vec<String>,
    /// Lines and bench events seen before the session had a work item; they
    /// move onto it when it gets one.
    #[serde(default)]
    pub held_lines: Vec<Line>,
    #[serde(default)]
    pub held_bench: Vec<BenchEvent>,
    #[serde(default)]
    pub ended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Span {
    pub item_id: Option<String>,
    pub date: String,
    pub first_ms: i64,
    pub last_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub ms: i64,
    pub text: String,
    /// `commit`, `agent` or `user`.
    pub source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarvestTarget {
    pub account_id: u64,
    pub project_id: u64,
    pub task_id: u64,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TileStatus {
    #[default]
    Open,
    Approved,
    Discarded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tile {
    pub id: String,
    pub item_id: String,
    pub date: String,
    #[serde(default)]
    pub lines: Vec<Line>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub hours_override: Option<f64>,
    #[serde(default)]
    pub harvest: Option<HarvestTarget>,
    #[serde(default)]
    pub status: TileStatus,
    #[serde(default)]
    pub harvest_entry_id: Option<u64>,
    #[serde(default)]
    pub acted_ms: Option<i64>,
    /// The seconds the tile carried when it was acted on, frozen so the
    /// record stays true after its sessions are pruned.
    #[serde(default)]
    pub final_seconds: Option<i64>,
}

/// A write an agent proposed. Prose never leaves the desk unseen (decision
/// 15): an agent's comment or new issue waits here until the user sends it,
/// as edited, from the Desk tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Draft {
    pub id: String,
    /// `linear_comment` or `linear_issue`.
    pub kind: String,
    /// The issue a comment answers.
    #[serde(default)]
    pub issue_key: Option<String>,
    /// The team a new issue goes to.
    #[serde(default)]
    pub team_key: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
    #[serde(default)]
    pub item_id: Option<String>,
    pub created_ms: i64,
    /// `open`, `sent` or `discarded`.
    pub status: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Timer {
    pub session_id: String,
    pub item_id: String,
    pub started_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingLaunch {
    pub item_id: String,
    pub repo: String,
    pub expires_ms: i64,
}

/// Local-calendar arithmetic, injected so tests do not depend on the
/// machine's timezone.
pub trait Days {
    fn date(&self, ms: i64) -> String;
    /// The first millisecond of the day after the one `ms` falls in.
    fn next_midnight(&self, ms: i64) -> i64;
}

pub struct LocalDays;

impl Days for LocalDays {
    fn date(&self, ms: i64) -> String {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_millis_opt(ms)
            .single()
            .map(|t| t.date_naive().to_string())
            .unwrap_or_default()
    }

    fn next_midnight(&self, ms: i64) -> i64 {
        use chrono::TimeZone;
        let Some(t) = chrono::Local.timestamp_millis_opt(ms).single() else {
            return ms + 86_400_000;
        };
        let next = t.date_naive() + chrono::Days::new(1);
        let midnight = next.and_hms_opt(0, 0, 0).expect("valid midnight");
        chrono::Local
            .from_local_datetime(&midnight)
            .earliest()
            .map(|d| d.timestamp_millis())
            .unwrap_or(ms + 86_400_000)
    }
}

/// Git, behind a trait: the desk asks a session's checkout for its branch,
/// its top level and its newest commit, and the tests answer for it.
pub trait Git {
    fn branch(&self, cwd: &str) -> Option<String>;
    fn toplevel(&self, cwd: &str) -> Option<String>;
    /// `(hash, subject)` of HEAD.
    fn head(&self, cwd: &str) -> Option<(String, String)>;
}

pub struct RealGit;

impl RealGit {
    fn run(cwd: &str, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

impl Git for RealGit {
    fn branch(&self, cwd: &str) -> Option<String> {
        Self::run(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
    }
    fn toplevel(&self, cwd: &str) -> Option<String> {
        Self::run(cwd, &["rev-parse", "--show-toplevel"])
    }
    fn head(&self, cwd: &str) -> Option<(String, String)> {
        let s = Self::run(cwd, &["log", "-1", "--format=%H%x1f%s"])?;
        let (h, subj) = s.split_once('\u{1f}')?;
        Some((h.to_string(), subj.to_string()))
    }
}

/// Is `path` inside one of the ignored prefixes? Matched on whole path
/// components, so `/a/embarch` does not swallow `/a/embarch-client`.
pub fn is_ignored(path: &str, ignore: &[String]) -> bool {
    let p = path.trim_end_matches('/');
    ignore.iter().any(|i| {
        let i = i.trim_end_matches('/');
        !i.is_empty() && (p == i || p.starts_with(&format!("{i}/")))
    })
}

/// The first `ABC-123`-shaped token in a branch name, uppercased — Linear's
/// own branch names carry one (`user/abc-123-bootload-over-smp`).
pub fn issue_key_in(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let start = i;
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            let letters = i - start;
            if i + 1 < b.len() && b[i] == b'-' && b[i + 1].is_ascii_digit() && (1..=10).contains(&letters) {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j == b.len() || !b[j].is_ascii_alphanumeric() {
                    return Some(text[start..j].to_ascii_uppercase());
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

impl State {
    pub fn new_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    pub fn item_by_key(&self, key: &str) -> Option<&WorkItem> {
        self.items
            .values()
            .filter(|i| !i.archived)
            .find(|i| i.issue.as_ref().is_some_and(|r| r.key.eq_ignore_ascii_case(key)))
    }

    pub fn create_item(&mut self, title: &str, issue: Option<IssueRef>, now: i64) -> String {
        let id = self.new_id("w");
        self.items.insert(
            id.clone(),
            WorkItem {
                id: id.clone(),
                title: title.to_string(),
                issue,
                repo_path: None,
                prs: vec![],
                bench: vec![],
                pinned: false,
                archived: false,
                created_ms: now,
                last_activity_ms: now,
            },
        );
        id
    }

    pub fn tile_mut(&mut self, item_id: &str, date: &str) -> &mut Tile {
        let id = format!("{item_id}@{date}");
        self.tiles.entry(id.clone()).or_insert_with(|| Tile {
            id,
            item_id: item_id.to_string(),
            date: date.to_string(),
            lines: vec![],
            note: None,
            hours_override: None,
            harvest: None,
            status: TileStatus::Open,
            harvest_entry_id: None,
            acted_ms: None,
            final_seconds: None,
        })
    }

    /// Session-open seconds on one item on one day: the plain sum of spans.
    pub fn tile_seconds(&self, item_id: &str, date: &str) -> i64 {
        if let Some(t) = self.tiles.get(&format!("{item_id}@{date}")) {
            if let Some(s) = t.final_seconds {
                return s;
            }
        }
        self.sessions
            .values()
            .filter(|s| !s.ignored)
            .flat_map(|s| s.spans.iter())
            .filter(|sp| sp.item_id.as_deref() == Some(item_id) && sp.date == date)
            .map(|sp| (sp.last_ms - sp.first_ms) / 1000)
            .sum()
    }

    /// Every (item, date) that has time or a tile record — the tiles that exist.
    pub fn tile_keys(&self) -> Vec<(String, String)> {
        let mut keys: std::collections::BTreeSet<(String, String)> = self
            .tiles
            .values()
            .map(|t| (t.item_id.clone(), t.date.clone()))
            .collect();
        for s in self.sessions.values().filter(|s| !s.ignored) {
            for sp in &s.spans {
                if let Some(item) = &sp.item_id {
                    keys.insert((item.clone(), sp.date.clone()));
                }
            }
        }
        keys.into_iter().collect()
    }

    pub fn add_line(&mut self, item_id: &str, date: &str, text: &str, source: &str, ms: i64) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.tile_mut(item_id, date).lines.push(Line {
            ms,
            text: text.to_string(),
            source: source.to_string(),
        });
        if let Some(item) = self.items.get_mut(item_id) {
            item.last_activity_ms = item.last_activity_ms.max(ms);
        }
    }

    pub fn add_bench(&mut self, item_id: &str, ev: BenchEvent) {
        if let Some(item) = self.items.get_mut(item_id) {
            item.last_activity_ms = item.last_activity_ms.max(ev.ms);
            item.bench.push(ev);
            let excess = item.bench.len().saturating_sub(200);
            item.bench.drain(..excess);
        }
    }

    /// Put a session on a work item. Spans it ran up while it had none — the
    /// minutes spent being asked — become the item's: over-capture. Moving
    /// from one item to another closes the old span at `now`.
    pub fn assign_session(
        &mut self,
        session_id: &str,
        item_id: &str,
        now: i64,
        days: &dyn Days,
        git: &dyn Git,
    ) -> bool {
        if !self.items.contains_key(item_id) {
            return false;
        }
        let Some(sess) = self.sessions.get_mut(session_id) else {
            return false;
        };
        sess.ignored = false;
        let switching = sess.item_id.as_deref().is_some_and(|cur| cur != item_id);
        if switching {
            record_activity(sess, now, days);
        }
        for sp in sess.spans.iter_mut().filter(|sp| sp.item_id.is_none()) {
            sp.item_id = Some(item_id.to_string());
        }
        sess.item_id = Some(item_id.to_string());
        if switching {
            sess.spans.push(Span {
                item_id: Some(item_id.to_string()),
                date: days.date(now),
                first_ms: now,
                last_ms: now,
            });
        }
        let held_lines = std::mem::take(&mut sess.held_lines);
        let held_bench = std::mem::take(&mut sess.held_bench);
        let cwd = sess.cwd.clone();
        for l in held_lines {
            self.add_line(item_id, &days.date(l.ms), &l.text, &l.source, l.ms);
        }
        for b in held_bench {
            self.add_bench(item_id, b);
        }
        if let Some(item) = self.items.get_mut(item_id) {
            item.last_activity_ms = item.last_activity_ms.max(now);
            if item.repo_path.is_none() {
                item.repo_path = git.toplevel(&cwd);
            }
        }
        true
    }

    pub fn is_timer_session(id: &str) -> bool {
        id.starts_with("timer-")
    }

    /// Start the manual timer on an item, stopping any running one first —
    /// one timer at a time, the way Harvest's own works.
    pub fn start_timer(&mut self, item_id: &str, now: i64, days: &dyn Days) -> bool {
        if !self.items.contains_key(item_id) {
            return false;
        }
        self.stop_timer(now, days);
        let sid = self.new_id("timer-");
        let mut sess = Session {
            id: sid.clone(),
            cwd: String::new(),
            item_id: Some(item_id.to_string()),
            ignored: false,
            started_ms: now,
            last_ms: now,
            spans: vec![],
            commits: vec![],
            held_lines: vec![],
            held_bench: vec![],
            ended: false,
        };
        record_activity(&mut sess, now, days);
        self.sessions.insert(sid.clone(), sess);
        self.timer = Some(Timer { session_id: sid, item_id: item_id.to_string(), started_ms: now });
        if let Some(i) = self.items.get_mut(item_id) {
            i.last_activity_ms = now;
        }
        true
    }

    /// Bring a running timer's span up to `now`. A timer is never idle, so
    /// the 8-hour gap rule must not end it: tick it as often as the view is read.
    pub fn tick_timer(&mut self, now: i64, days: &dyn Days) {
        let Some(t) = self.timer.clone() else { return };
        if let Some(s) = self.sessions.get_mut(&t.session_id) {
            let last = s.last_ms;
            // Fill the gap in steps under the cap, so a long-running timer
            // read rarely still counts in full.
            let mut at = last;
            while now - at > MAX_GAP_MS {
                at += MAX_GAP_MS;
                record_activity(s, at, days);
            }
            record_activity(s, now, days);
        }
    }

    pub fn stop_timer(&mut self, now: i64, days: &dyn Days) -> Option<Timer> {
        self.tick_timer(now, days);
        let t = self.timer.take()?;
        if let Some(s) = self.sessions.get_mut(&t.session_id) {
            s.ended = true;
        }
        Some(t)
    }

    pub fn untrack_session(&mut self, session_id: &str) -> bool {
        match self.sessions.get_mut(session_id) {
            Some(s) => {
                s.ignored = true;
                s.item_id = None;
                s.spans.clear();
                s.held_lines.clear();
                s.held_bench.clear();
                true
            }
            None => false,
        }
    }

    /// Move acted-on tiles past [`ARCHIVE_AFTER_MS`] out of the live state
    /// and drop sessions nothing open depends on. Returns the archived tiles
    /// for the caller to append to history.
    pub fn prune(&mut self, now: i64) -> Vec<Tile> {
        let old: Vec<String> = self
            .tiles
            .values()
            .filter(|t| t.status != TileStatus::Open && t.acted_ms.is_some_and(|a| now - a > ARCHIVE_AFTER_MS))
            .map(|t| t.id.clone())
            .collect();
        let archived: Vec<Tile> = old.iter().filter_map(|id| self.tiles.remove(id)).collect();
        let open: std::collections::BTreeSet<(String, String)> = self
            .tile_keys()
            .into_iter()
            .filter(|(i, d)| {
                self.tiles
                    .get(&format!("{i}@{d}"))
                    .is_none_or(|t| t.status == TileStatus::Open)
            })
            .collect();
        self.sessions.retain(|_, s| {
            now - s.last_ms < SESSION_TTL_MS
                || s.spans.iter().any(|sp| {
                    sp.item_id
                        .as_ref()
                        .is_some_and(|i| open.contains(&(i.clone(), sp.date.clone())))
                })
        });
        self.pending_launches.retain(|p| p.expires_ms > now);
        archived
    }
}

/// Extend the session's current span to `ms`, or open a new one: on a new
/// local day (splitting at midnight when activity runs across it), after a
/// gap longer than [`MAX_GAP_MS`], or when the work item changed.
pub fn record_activity(sess: &mut Session, ms: i64, days: &dyn Days) {
    sess.last_ms = sess.last_ms.max(ms);
    let item = sess.item_id.clone();
    let Some(last) = sess.spans.last_mut() else {
        sess.spans.push(Span { item_id: item, date: days.date(ms), first_ms: ms, last_ms: ms });
        return;
    };
    if last.item_id != item || ms - last.last_ms > MAX_GAP_MS {
        sess.spans.push(Span { item_id: item, date: days.date(ms), first_ms: ms, last_ms: ms });
        return;
    }
    if ms <= last.last_ms {
        return;
    }
    if last.date == days.date(ms) {
        last.last_ms = ms;
        return;
    }
    // Continuous activity across one or more midnights: close each day at
    // its last millisecond and open the next at its first.
    let mut cursor = days.next_midnight(last.last_ms);
    last.last_ms = cursor - 1;
    while days.date(cursor) != days.date(ms) {
        let end = days.next_midnight(cursor);
        sess.spans.push(Span { item_id: item.clone(), date: days.date(cursor), first_ms: cursor, last_ms: end - 1 });
        cursor = end;
    }
    sess.spans.push(Span { item_id: item, date: days.date(ms), first_ms: cursor, last_ms: ms });
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// UTC days, whatever the machine's zone is.
    pub struct UtcDays;
    impl Days for UtcDays {
        fn date(&self, ms: i64) -> String {
            chrono::DateTime::from_timestamp_millis(ms).unwrap().date_naive().to_string()
        }
        fn next_midnight(&self, ms: i64) -> i64 {
            (ms.div_euclid(86_400_000) + 1) * 86_400_000
        }
    }

    #[derive(Default)]
    pub struct FakeGit {
        pub branch: Option<String>,
        pub toplevel: Option<String>,
        pub head: std::cell::RefCell<Option<(String, String)>>,
    }
    impl Git for FakeGit {
        fn branch(&self, _: &str) -> Option<String> {
            self.branch.clone()
        }
        fn toplevel(&self, _: &str) -> Option<String> {
            self.toplevel.clone()
        }
        fn head(&self, _: &str) -> Option<(String, String)> {
            self.head.borrow().clone()
        }
    }

    pub fn session(id: &str) -> Session {
        Session {
            id: id.into(),
            cwd: "/w/repo".into(),
            item_id: None,
            ignored: false,
            started_ms: 0,
            last_ms: 0,
            spans: vec![],
            commits: vec![],
            held_lines: vec![],
            held_bench: vec![],
            ended: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const H: i64 = 3_600_000;
    const DAY: i64 = 24 * H;
    // 2026-10-22T00:00:00Z
    const D0: i64 = 1_792_627_200_000;

    #[test]
    fn idle_gaps_under_the_cap_count_as_session_time() {
        let mut s = session("s");
        s.item_id = Some("w1".into());
        record_activity(&mut s, D0 + 9 * H, &UtcDays);
        record_activity(&mut s, D0 + 12 * H, &UtcDays); // three hours idle: counts
        assert_eq!(s.spans.len(), 1);
        assert_eq!(s.spans[0].last_ms - s.spans[0].first_ms, 3 * H);
    }

    #[test]
    fn a_window_left_open_overnight_is_not_a_night_of_work() {
        let mut s = session("s");
        s.item_id = Some("w1".into());
        record_activity(&mut s, D0 + 17 * H, &UtcDays);
        record_activity(&mut s, D0 + DAY + 9 * H, &UtcDays);
        assert_eq!(s.spans.len(), 2);
        assert_eq!(s.spans[0].last_ms, D0 + 17 * H);
        assert_eq!(s.spans[1].first_ms, D0 + DAY + 9 * H);
    }

    #[test]
    fn activity_across_midnight_splits_into_two_days() {
        let mut s = session("s");
        s.item_id = Some("w1".into());
        record_activity(&mut s, D0 + 23 * H, &UtcDays);
        record_activity(&mut s, D0 + DAY + H, &UtcDays);
        assert_eq!(s.spans.len(), 2);
        assert_eq!(s.spans[0].date, "2026-10-22");
        assert_eq!(s.spans[0].last_ms, D0 + DAY - 1);
        assert_eq!(s.spans[1].date, "2026-10-23");
        assert_eq!(s.spans[1].first_ms, D0 + DAY);
    }

    #[test]
    fn time_spent_being_asked_lands_on_the_item_chosen() {
        let mut st = State::default();
        let w = st.create_item("Bootload", None, D0);
        let mut s = session("s");
        record_activity(&mut s, D0 + 9 * H, &UtcDays);
        record_activity(&mut s, D0 + 10 * H, &UtcDays);
        st.sessions.insert("s".into(), s);
        assert_eq!(st.tile_seconds(&w, "2026-10-22"), 0);
        assert!(st.assign_session("s", &w, D0 + 10 * H, &UtcDays, &FakeGit::default()));
        assert_eq!(st.tile_seconds(&w, "2026-10-22"), 3600);
    }

    #[test]
    fn parallel_sessions_on_one_item_both_count_in_full() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        for id in ["a", "b"] {
            let mut s = session(id);
            s.item_id = Some(w.clone());
            record_activity(&mut s, D0 + 9 * H, &UtcDays);
            record_activity(&mut s, D0 + 11 * H, &UtcDays);
            st.sessions.insert(id.into(), s);
        }
        assert_eq!(st.tile_seconds(&w, "2026-10-22"), 4 * 3600);
    }

    #[test]
    fn switching_items_closes_the_old_span_at_the_switch() {
        let mut st = State::default();
        let a = st.create_item("a", None, D0);
        let b = st.create_item("b", None, D0);
        let mut s = session("s");
        s.item_id = Some(a.clone());
        record_activity(&mut s, D0 + 9 * H, &UtcDays);
        st.sessions.insert("s".into(), s);
        st.assign_session("s", &b, D0 + 10 * H, &UtcDays, &FakeGit::default());
        record_activity(st.sessions.get_mut("s").unwrap(), D0 + 12 * H, &UtcDays);
        assert_eq!(st.tile_seconds(&a, "2026-10-22"), 3600);
        assert_eq!(st.tile_seconds(&b, "2026-10-22"), 2 * 3600);
    }

    #[test]
    fn held_lines_and_bench_follow_the_session_onto_its_item() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        let mut s = session("s");
        s.held_lines.push(Line { ms: D0 + H, text: "fix the race".into(), source: "commit".into() });
        s.held_bench.push(BenchEvent { ms: D0 + H, tool: "flash".into(), ok: true });
        st.sessions.insert("s".into(), s);
        let git = FakeGit { toplevel: Some("/w/repo".into()), ..Default::default() };
        st.assign_session("s", &w, D0 + 2 * H, &UtcDays, &git);
        assert_eq!(st.tiles[&format!("{w}@2026-10-22")].lines[0].text, "fix the race");
        assert_eq!(st.items[&w].bench.len(), 1);
        assert_eq!(st.items[&w].repo_path.as_deref(), Some("/w/repo"));
    }

    #[test]
    fn an_acted_tile_keeps_its_seconds_after_sessions_go() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        let t = st.tile_mut(&w, "2026-10-22");
        t.status = TileStatus::Approved;
        t.final_seconds = Some(5400);
        t.acted_ms = Some(D0);
        assert_eq!(st.tile_seconds(&w, "2026-10-22"), 5400);
        let archived = st.prune(D0 + 8 * DAY);
        assert_eq!(archived.len(), 1);
        assert!(st.tiles.is_empty());
    }

    #[test]
    fn a_timer_counts_until_stopped_and_one_runs_at_a_time() {
        let mut st = State::default();
        let a = st.create_item("Bench rework", None, D0);
        let b = st.create_item("Client call", None, D0);
        assert!(st.start_timer(&a, D0 + 9 * H, &UtcDays));
        st.tick_timer(D0 + 10 * H, &UtcDays);
        assert_eq!(st.tile_seconds(&a, "2026-10-22"), 3600);
        assert!(st.start_timer(&b, D0 + 11 * H, &UtcDays));
        assert_eq!(st.tile_seconds(&a, "2026-10-22"), 2 * 3600, "starting b stopped a at 11:00");
        st.stop_timer(D0 + 11 * H + 1_800_000, &UtcDays);
        assert_eq!(st.tile_seconds(&b, "2026-10-22"), 1800);
        assert!(st.timer.is_none());
        assert!(!st.start_timer("nope", D0, &UtcDays));
    }

    #[test]
    fn a_timer_left_running_past_the_gap_cap_still_counts_in_full() {
        let mut st = State::default();
        let a = st.create_item("Long bench day", None, D0);
        st.start_timer(&a, D0 + H, &UtcDays);
        st.stop_timer(D0 + 11 * H, &UtcDays);
        assert_eq!(st.tile_seconds(&a, "2026-10-22"), 10 * 3600);
    }

    #[test]
    fn ignore_matches_whole_components_only() {
        let ig = vec!["/home/g/Github/embarch".to_string()];
        assert!(is_ignored("/home/g/Github/embarch", &ig));
        assert!(is_ignored("/home/g/Github/embarch/embarch-ui/src", &ig));
        assert!(!is_ignored("/home/g/Github/embarch-client", &ig));
        assert!(!is_ignored("/home/g/Github", &ig));
    }

    #[test]
    fn issue_keys_come_out_of_linear_branch_names() {
        assert_eq!(issue_key_in("gabriel/abc-123-bootload-over-smp").as_deref(), Some("ABC-123"));
        assert_eq!(issue_key_in("ABC-7").as_deref(), Some("ABC-7"));
        assert_eq!(issue_key_in("feature/abc-12x"), None);
        assert_eq!(issue_key_in("main"), None);
        assert_eq!(issue_key_in("fix: x2-3 then ws-12 done").as_deref(), Some("WS-12"));
    }
}
