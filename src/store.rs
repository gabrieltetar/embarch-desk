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
/// Time captured after a ✓ under which no continuation is dealt (decision
/// 64): the session's own last events after the click are not more work.
pub const MORE_MIN_SECONDS: i64 = 5 * 60;

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
    /// One desk per project: a Linear project, a Harvest project, its GitHub
    /// repos, firmware repos and saved bench (`embarch-open-desk-proposal.md`).
    #[serde(default)]
    pub desks: BTreeMap<String, Desk>,
    /// The one desk the whole suite has open, or none — a bare repo, nothing
    /// scoped.
    #[serde(default)]
    pub open_desk: Option<String>,
    /// What Harvest already holds, per day: entries logged by hand, by the
    /// Harvest app, or by a tile's ✓. A tile proposes only the gap.
    #[serde(default)]
    pub harvest_logged: BTreeMap<String, Vec<LoggedEntry>>,
    /// Notes kept on the work item itself rather than on a day's tile, by
    /// item id: captured memory files, every version, and the lines of tiles
    /// already archived — the work's knowledge outlives its tiles
    /// (`embarch-chat-desk-proposal.md` 40).
    #[serde(default)]
    pub notes: BTreeMap<String, Vec<Line>>,
    /// When each item's notes last changed — a note added, a memory file's
    /// new version, held notes landing, the import, an archived tile's lines
    /// moving onto the item. A card re-reads its notes only when this moves
    /// (chat-desk proposal, Interface); a held note lands with its own older
    /// time, so the newest note's time would not. A state file from before
    /// it reads the newest note's time until the item's next change.
    #[serde(default)]
    pub notes_changed: BTreeMap<String, i64>,
    /// The memory files written before capture existed have been attached
    /// to their items (proposal 47) — once, on the first start with notes.
    #[serde(default)]
    pub memory_imported: bool,
    /// Requests for the embarch-chat panel: start a chat, or focus one, in
    /// the window whose folder matches (proposal 33).
    #[serde(default)]
    pub mailbox: Vec<MailboxEntry>,
    #[serde(default)]
    pub settings: Settings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Start work leaves its start in the mailbox for the embarch-chat panel
    /// instead of pre-filling Claude Code's. Off until the chat ships (chat
    /// proposal decision 4).
    #[serde(default)]
    pub chat_panel: bool,
}

/// One request waiting for a VS Code window's embarch-chat panel, taken once
/// by the window whose folder matches — decision 25's launch claim extended
/// to the panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailboxEntry {
    pub id: String,
    /// `start` (a chat on an item, with a prompt) or `focus` (a session).
    pub kind: String,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub folder: String,
    #[serde(default)]
    pub prompt: Option<String>,
    pub created_ms: i64,
    pub expires_ms: i64,
    /// Kept until it expires, so a second take is told it is gone.
    #[serde(default)]
    pub taken_ms: Option<i64>,
}

/// Why a mailbox entry could not be taken.
#[derive(Debug, PartialEq)]
pub enum TakeError {
    Unknown,
    Taken,
    Expired,
}

/// The kinds a session or the user may give a note. `memory` is the
/// desk's own: a captured memory file.
pub const NOTE_KINDS: &[&str] = &["done", "finding", "decision", "handoff", "todo"];
/// A captured memory file keeps this many versions; older ones go.
pub const MEMORY_VERSIONS_KEPT: usize = 20;
/// A memory file is kept up to this many characters.
pub const MEMORY_TEXT_MAX: usize = 32_000;

/// A memory file on disk, as the import reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryFile {
    pub path: String,
    pub mtime_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LoggedEntry {
    pub id: u64,
    pub account_id: u64,
    pub project_id: u64,
    pub task_id: u64,
    pub client: String,
    pub project: String,
    pub task: String,
    pub notes: String,
    pub hours: f64,
    #[serde(default)]
    pub running: bool,
    /// The issue key the note names, if any.
    #[serde(default)]
    pub issue_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LinearBinding {
    /// The Linear connector account label the project lives under.
    pub account: String,
    pub project_id: String,
    pub project_name: String,
    #[serde(default)]
    pub team_id: String,
    #[serde(default)]
    pub team_key: String,
}

impl LinearBinding {
    pub fn holds(&self, issue: &IssueRef) -> bool {
        if !self.project_id.is_empty() {
            !issue.project_id.is_empty() && issue.project_id == self.project_id
        } else {
            !self.team_id.is_empty() && issue.team_id == self.team_id
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Desk {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub linear: Option<LinearBinding>,
    /// Every tile's default Harvest project and task (Intercreate's account).
    #[serde(default)]
    pub harvest: Option<HarvestTarget>,
    /// `owner/repo`, where its PRs are looked for.
    #[serde(default)]
    pub github_repos: Vec<String>,
    /// Absolute paths; the first is what the suite opens.
    #[serde(default)]
    pub firmware_repos: Vec<String>,
    /// A saved bench in the firmware repo, proposed when the desk opens.
    #[serde(default)]
    pub bench: Option<String>,
    #[serde(default)]
    pub created_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    /// The desk it belongs to: an issue's is the desk holding its Linear
    /// project, a standalone item's is chosen when it is made.
    #[serde(default)]
    pub desk_id: Option<String>,
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
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub project_name: String,
    /// Linear's git branch name for the issue, which Start work checks out.
    #[serde(default)]
    pub branch_name: String,
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
    /// The desk whose firmware repo the session started in.
    #[serde(default)]
    pub desk_id: Option<String>,
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
    /// What Claude last said (the Stop hook's `last_assistant_message`).
    #[serde(default)]
    pub last_reply: Option<String>,
    #[serde(default)]
    pub last_reply_ms: i64,
    /// Set by a Notification hook (a permission prompt, an idle prompt),
    /// cleared by the next prompt or tool call.
    #[serde(default)]
    pub waiting: Option<String>,
    /// Between a prompt or a tool call and the Stop that ends the turn.
    #[serde(default)]
    pub busy: bool,
    /// What runs the session when it says so on assign: `embarch-chat` for
    /// the panel's chats.
    #[serde(default)]
    pub client: Option<String>,
}

impl Session {
    pub fn new(id: &str, cwd: &str, now: i64) -> Self {
        Session {
            id: id.to_string(),
            cwd: cwd.to_string(),
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
            last_reply: None,
            last_reply_ms: 0,
            waiting: None,
            busy: false,
            client: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Span {
    pub item_id: Option<String>,
    pub date: String,
    pub first_ms: i64,
    pub last_ms: i64,
}

/// A note — notes and tile lines are one stream (proposal 43): a tile line
/// is a note of kind `done`. Lines written before notes existed read as
/// `done` and get an id when the state loads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub ms: i64,
    pub text: String,
    /// `commit`, `agent`, `user` or `memory`.
    pub source: String,
    #[serde(default)]
    pub id: String,
    /// `done`, `finding`, `decision`, `handoff`, `todo`, or `memory` for a
    /// captured memory file.
    #[serde(default = "kind_done")]
    pub kind: String,
    #[serde(default)]
    pub session_id: Option<String>,
    /// A memory note's file: one note per file, its versions kept.
    #[serde(default)]
    pub source_path: Option<String>,
}

fn kind_done() -> String {
    "done".to_string()
}

impl Line {
    /// A plain tile line: kind `done`, no session.
    pub fn new(ms: i64, text: &str, source: &str) -> Self {
        Line {
            ms,
            text: text.to_string(),
            source: source.to_string(),
            id: String::new(),
            kind: kind_done(),
            session_id: None,
            source_path: None,
        }
    }

    /// A version of a captured memory file.
    pub fn memory(ms: i64, path: &str, text: &str) -> Self {
        let mut l = Line::new(ms, text, "memory");
        l.kind = "memory".into();
        l.source_path = Some(path.to_string());
        l
    }

    pub fn is_memory(&self) -> bool {
        self.kind == "memory"
    }
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
    /// Its time and lines went into a later tile of the same item, which
    /// sends them as one Harvest entry.
    Folded,
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
    /// Hours a person typed (decision 57). Only that: nothing the desk
    /// computed or sent ever lands here (decision 61).
    #[serde(default)]
    pub hours_override: Option<f64>,
    /// The hours ✓ sent to Harvest, kept so the approved record reads true.
    /// Cleared when the entry goes and the tile reopens (decision 61).
    #[serde(default)]
    pub sent_hours: Option<f64>,
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
    /// The date of the tile this one was folded into, same item.
    #[serde(default)]
    pub folded_into: Option<String>,
    /// Hours a person typed for an approved tile's continuation (decision 64).
    #[serde(default)]
    pub more_hours: Option<f64>,
    /// The note drafted for the entry a continuation's ✓ updates; the sent
    /// one stays in `note` until then.
    #[serde(default)]
    pub more_note: Option<String>,
    /// Harvest's hours for the item that day right after the last ✓, so time
    /// typed into Harvest since is not proposed again.
    #[serde(default)]
    pub harvest_at_send: Option<f64>,
}

/// What an approved tile captured since its ✓ (decision 64), dealt again as
/// a continuation whose ✓ adds to the same Harvest entry.
#[derive(Debug, Clone, PartialEq)]
pub struct More {
    pub seconds: i64,
    /// Those seconds less what Harvest took on for the item since the ✓.
    pub computed_hours: f64,
    /// What the continuation proposes: the typed hours, else the computed.
    pub hours: f64,
    /// The entry's hours now: Harvest's, else what ✓ sent.
    pub entry_hours: f64,
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
    /// The Linear project a new issue is filed into (the open desk's).
    #[serde(default)]
    pub project_id: Option<String>,
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
    /// `2026-10-22 14:03`, for text a session reads.
    fn stamp(&self, ms: i64) -> String;
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

    fn stamp(&self, ms: i64) -> String {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_millis_opt(ms)
            .single()
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
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

/// `YYYY-MM-DD`, a real calendar day, nothing looser (`2026-1-5` is not one).
pub fn is_date(s: &str) -> bool {
    s.len() == 10 && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

/// A folder as the mailbox compares it: no trailing slash.
pub fn norm_folder(path: &str) -> String {
    let p = path.trim();
    match p.trim_end_matches('/') {
        "" if p.starts_with('/') => "/".to_string(),
        t => t.to_string(),
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
    issue_keys_in(text).into_iter().next()
}

/// Every `abc123`-shaped token — a key with its dash dropped, the way file
/// names often carry one — as `ABC-123`, in order, once each.
pub fn compact_keys_in(text: &str) -> Vec<String> {
    let mut keys: Vec<String> = vec![];
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let start = i;
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            let letters = i - start;
            let digits_at = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if (1..=10).contains(&letters) && i > digits_at && (i == b.len() || !b[i].is_ascii_alphanumeric()) {
                let key = format!("{}-{}", text[start..digits_at].to_ascii_uppercase(), &text[digits_at..i]);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        } else {
            i += 1;
        }
    }
    keys
}

/// A memory file's frontmatter, between its opening and closing `---`;
/// empty when it has none.
pub fn frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else { return "" };
    rest.find("\n---").map_or("", |end| &rest[..end])
}

/// Every `ABC-123`-shaped token in a text, uppercased, in order, once each.
pub fn issue_keys_in(text: &str) -> Vec<String> {
    let mut keys: Vec<String> = vec![];
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
                    let key = text[start..j].to_ascii_uppercase();
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                    i = j;
                }
            }
        } else {
            i += 1;
        }
    }
    keys
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
        let desk_id = issue.as_ref().and_then(|i| self.desk_for_issue(i));
        let id = self.new_id("w");
        self.items.insert(
            id.clone(),
            WorkItem {
                id: id.clone(),
                title: title.to_string(),
                desk_id,
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

    /// The entry a ✓ made is gone: its tiles open again with what ✓ froze
    /// cleared, so they propose the current gap. A person's typed override
    /// stays (decision 61).
    pub fn reopen_tiles_of_entry(&mut self, entry: u64) {
        for t in self.tiles.values_mut().filter(|t| t.harvest_entry_id == Some(entry)) {
            t.status = TileStatus::Open;
            t.harvest_entry_id = None;
            t.acted_ms = None;
            t.final_seconds = None;
            t.sent_hours = None;
            t.more_hours = None;
            t.more_note = None;
            t.harvest_at_send = None;
        }
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
            sent_hours: None,
            harvest: None,
            status: TileStatus::Open,
            harvest_entry_id: None,
            acted_ms: None,
            final_seconds: None,
            folded_into: None,
            more_hours: None,
            more_note: None,
            harvest_at_send: None,
        })
    }

    /// Session-open seconds on one item on one day: the plain sum of spans,
    /// plus whatever earlier tiles were folded into this one.
    pub fn tile_seconds(&self, item_id: &str, date: &str) -> i64 {
        if let Some(t) = self.tiles.get(&format!("{item_id}@{date}")) {
            if let Some(s) = t.final_seconds {
                return s;
            }
        }
        self.live_seconds(item_id, date)
    }

    /// [`State::tile_seconds`] as the sessions have it now, past any freeze.
    pub fn live_seconds(&self, item_id: &str, date: &str) -> i64 {
        let own: i64 = self
            .sessions
            .values()
            .filter(|s| !s.ignored)
            .flat_map(|s| s.spans.iter())
            .filter(|sp| sp.item_id.as_deref() == Some(item_id) && sp.date == date)
            .map(|sp| (sp.last_ms - sp.first_ms) / 1000)
            .sum();
        own + self.folded_into(item_id, date).map(|t| t.final_seconds.unwrap_or(0)).sum::<i64>()
    }

    /// The tiles of an item folded into its tile on `date`.
    pub fn folded_into<'a>(&'a self, item_id: &'a str, date: &'a str) -> impl Iterator<Item = &'a Tile> + 'a {
        self.tiles.values().filter(move |t| {
            t.item_id == item_id && t.status == TileStatus::Folded && t.folded_into.as_deref() == Some(date)
        })
    }

    /// Fold an item's open tile on `from` into its open tile on `into`: the
    /// time and the lines go with it, and one Harvest entry dated `into`
    /// carries both. What it carries is what it proposed — its own hours if
    /// the user set them, else the gap Harvest's entries that day left — so
    /// no hour is sent twice. That freezes now as its `final_seconds`.
    pub fn fold_tile(&mut self, item_id: &str, from: &str, into: &str, now: i64) -> Result<(), String> {
        if !is_date(from) || !is_date(into) {
            return Err("a fold names two dates, YYYY-MM-DD".into());
        }
        if into <= from {
            return Err("a tile folds into a later day, never the same or an earlier one".into());
        }
        for d in [from, into] {
            let open = self.tiles.get(&format!("{item_id}@{d}")).is_none_or(|t| t.status == TileStatus::Open);
            if !open || !self.tile_keys().contains(&(item_id.to_string(), d.to_string())) {
                return Err(format!("no open tile on {d} for this item"));
            }
        }
        let captured = self.tile_seconds(item_id, from);
        let logged = (self.logged_hours(item_id, from) * 3600.0).round() as i64;
        let seconds = match self.tiles.get(&format!("{item_id}@{from}")).and_then(|t| t.hours_override) {
            Some(h) => (h * 3600.0).round() as i64,
            None => (captured - logged).max(0),
        };
        // The target gets a record, so the fold outlives its sessions.
        self.tile_mut(item_id, into);
        let t = self.tile_mut(item_id, from);
        t.status = TileStatus::Folded;
        t.folded_into = Some(into.to_string());
        t.acted_ms = Some(now);
        t.final_seconds = Some(seconds);
        Ok(())
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
        self.add_note(item_id, date, Line::new(ms, text, source));
    }

    /// File a note on an item: a captured memory file on the item itself,
    /// anything else on the day's tile as a line. Returns its id, or `None`
    /// for an empty one. The note is taken now, at its own time.
    pub fn add_note(&mut self, item_id: &str, date: &str, note: Line) -> Option<String> {
        let ms = note.ms;
        self.land_note(item_id, date, note, ms)
    }

    /// [`State::add_note`] for a note landing at `now`, later than it was
    /// taken: a held one.
    fn land_note(&mut self, item_id: &str, date: &str, note: Line, now: i64) -> Option<String> {
        let ms = note.ms;
        let id = self.file_note(item_id, date, note, now)?;
        if let Some(item) = self.items.get_mut(item_id) {
            item.last_activity_ms = item.last_activity_ms.max(ms);
        }
        Some(id)
    }

    /// [`State::land_note`] without counting the note as activity on the item.
    fn file_note(&mut self, item_id: &str, date: &str, mut note: Line, now: i64) -> Option<String> {
        note.text = note.text.trim().to_string();
        if note.text.is_empty() {
            return None;
        }
        if note.id.is_empty() {
            note.id = self.new_id("n");
        }
        let id = note.id.clone();
        let before = self.notes_ms(item_id);
        let changed = if note.is_memory() {
            self.add_memory(item_id, note)
        } else {
            self.tile_mut(item_id, date).lines.push(note);
            true
        };
        if changed {
            self.notes_moved(item_id, before, now);
        }
        Some(id)
    }

    /// The item's notes changed at `now`; `before` is what [`State::notes_ms`]
    /// read ahead of the change. Always later than `before`, so a client
    /// keyed on it re-reads even when two changes share a millisecond.
    fn notes_moved(&mut self, item_id: &str, before: Option<i64>, now: i64) {
        let at = before.map_or(now, |b| now.max(b + 1));
        self.notes_changed.insert(item_id.to_string(), at);
    }

    /// A new version of a memory note. The same text twice is one version;
    /// past [`MEMORY_VERSIONS_KEPT`] the oldest goes. False when nothing
    /// changed.
    fn add_memory(&mut self, item_id: &str, mut note: Line) -> bool {
        if note.text.chars().count() > MEMORY_TEXT_MAX {
            note.text = note.text.chars().take(MEMORY_TEXT_MAX).collect();
        }
        let kept = self.notes.entry(item_id.to_string()).or_default();
        let same_file = |l: &Line| l.is_memory() && l.source_path == note.source_path;
        if kept.iter().rev().find(|l| same_file(l)).is_some_and(|l| l.text == note.text) {
            return false;
        }
        let path = note.source_path.clone();
        kept.push(note);
        let versions = kept.iter().filter(|l| l.is_memory() && l.source_path == path).count();
        if versions > MEMORY_VERSIONS_KEPT {
            if let Some(pos) = kept.iter().position(|l| l.is_memory() && l.source_path == path) {
                kept.remove(pos);
            }
        }
        true
    }

    /// Every note on an item, newest first: its tiles' lines, its memory
    /// files' versions and the lines of its archived tiles.
    pub fn item_notes(&self, item_id: &str) -> Vec<&Line> {
        let mut out: Vec<&Line> = self.notes_of(item_id).collect();
        out.sort_by_key(|l| std::cmp::Reverse(l.ms));
        out
    }

    fn notes_of<'a>(&'a self, item_id: &str) -> impl Iterator<Item = &'a Line> + 'a {
        let id = item_id.to_string();
        self.tiles
            .values()
            .filter(move |t| t.item_id == id)
            .flat_map(|t| t.lines.iter())
            .chain(self.notes.get(item_id).into_iter().flatten())
    }

    /// When the item's notes last changed: a card re-reads its notes only
    /// when this moves (chat-desk proposal, Interface). An item no change
    /// has been recorded on yet — notes from a state file older than the
    /// record — reads its newest note's time.
    pub fn notes_ms(&self, item_id: &str) -> Option<i64> {
        self.notes_changed.get(item_id).copied().or_else(|| self.notes_of(item_id).map(|l| l.ms).max())
    }

    /// Each memory file's newest version on an item, newest first.
    pub fn memory_latest(&self, item_id: &str) -> Vec<&Line> {
        let mut seen: Vec<&Option<String>> = vec![];
        let mut out = vec![];
        for l in self.item_notes(item_id).into_iter().filter(|l| l.is_memory()) {
            if !seen.contains(&&l.source_path) {
                seen.push(&l.source_path);
                out.push(l);
            }
        }
        out
    }

    /// A day's notes on an item, every kind, oldest first: what the Harvest
    /// note summarises (proposal 43). The day's tile's lines are its own
    /// whenever they were added — tiles stay open, so a line added to
    /// yesterday's this morning is yesterday's; the memory notes kept on
    /// the item itself count on the day they were taken. The only other
    /// notes kept there are archived tiles' lines, whose day was sent or
    /// discarded a week before, so none of them is any later day's work. A
    /// memory file written several times that day is one note, as it stood
    /// last.
    pub fn day_notes(&self, item_id: &str, date: &str, days: &dyn Days) -> Vec<Line> {
        let mut kept: Vec<&Line> = self
            .notes
            .get(item_id)
            .into_iter()
            .flatten()
            .filter(|l| l.is_memory() && days.date(l.ms) == date)
            .collect();
        kept.sort_by_key(|l| std::cmp::Reverse(l.ms));
        let mut seen = std::collections::HashSet::new();
        kept.retain(|l| seen.insert(&l.source_path));
        let tile = self.tiles.get(&format!("{item_id}@{date}")).into_iter().flat_map(|t| t.lines.iter());
        let folded = self.folded_into(item_id, date).flat_map(|t| t.lines.iter());
        let mut out: Vec<Line> = tile.chain(folded).chain(kept).cloned().collect();
        out.sort_by_key(|l| l.ms);
        out
    }

    /// Lines from before notes existed get their ids. Their kind already
    /// reads as `done`; nothing else changes. True when it gave any.
    pub fn migrate(&mut self) -> bool {
        let before = self.next_id;
        let mut next = self.next_id;
        let mut give = |l: &mut Line| {
            if l.id.is_empty() {
                next += 1;
                l.id = format!("n{next}");
            }
        };
        for t in self.tiles.values_mut() {
            t.lines.iter_mut().for_each(&mut give);
        }
        for s in self.sessions.values_mut() {
            s.held_lines.iter_mut().for_each(&mut give);
        }
        for n in self.notes.values_mut() {
            n.iter_mut().for_each(&mut give);
        }
        self.next_id = next;
        next != before
    }

    /// Attach the memory files written before capture to the items whose
    /// issue key they hold (proposal 47): a key in the file's name names its
    /// item alone; else every item whose key its text holds. A file already
    /// on an item is left alone, so a second run adds nothing. An attached
    /// file is not activity on the item: last month's state file must not
    /// bring a quiet card back to the Desk, though the item's notes changed
    /// `now`. Returns the notes added.
    pub fn import_memories(&mut self, files: &[MemoryFile], now: i64) -> usize {
        let mut added = 0;
        for f in files {
            let name = std::path::Path::new(&f.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // The file's name, else its frontmatter — the agent's own summary
            // of what the file is about — and one item at most: a body that
            // mentions three issues in passing is about none of them.
            let item_for = |text: &str| -> Option<String> {
                issue_keys_in(text).into_iter().chain(compact_keys_in(text)).find_map(|k| self.item_by_key(&k).map(|i| i.id.clone()))
            };
            let Some(item) = item_for(&name).or_else(|| item_for(frontmatter(&f.text))) else { continue };
            let has = self.notes.get(&item).is_some_and(|n| n.iter().any(|l| l.source_path.as_deref() == Some(f.path.as_str())));
            if has {
                continue;
            }
            if self.file_note(&item, "", Line::memory(f.mtime_ms, &f.path, &f.text), now).is_some() {
                added += 1;
            }
        }
        self.memory_imported = true;
        added
    }

    /// Leave a request for the window whose folder is `folder`. A start
    /// replaces one for the same item and folder the window has not taken
    /// yet: Start work pressed twice while the window opens is one chat.
    pub fn post_mail(&mut self, mut entry: MailboxEntry, now: i64) -> String {
        entry.id = self.new_id("m");
        entry.folder = norm_folder(&entry.folder);
        if entry.kind == "start" {
            self.mailbox.retain(|m| m.kind != "start" || m.taken_ms.is_some() || m.item_id != entry.item_id || m.folder != entry.folder);
        }
        entry.created_ms = now;
        entry.expires_ms = now + LAUNCH_CLAIM_MS;
        entry.taken_ms = None;
        let id = entry.id.clone();
        self.mailbox.push(entry);
        id
    }

    /// The entries still waiting for a window on `folder`.
    pub fn mail_for(&self, folder: &str, now: i64) -> Vec<&MailboxEntry> {
        let folder = norm_folder(folder);
        self.mailbox
            .iter()
            .filter(|m| m.taken_ms.is_none() && m.expires_ms > now && m.folder == folder)
            .collect()
    }

    /// Take an entry, once.
    pub fn take_mail(&mut self, id: &str, now: i64) -> Result<MailboxEntry, TakeError> {
        let m = self.mailbox.iter_mut().find(|m| m.id == id).ok_or(TakeError::Unknown)?;
        if m.taken_ms.is_some() {
            return Err(TakeError::Taken);
        }
        if m.expires_ms <= now {
            return Err(TakeError::Expired);
        }
        m.taken_ms = Some(now);
        Ok(m.clone())
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
        // Each lands now, keeping the time it was taken.
        for l in held_lines {
            self.land_note(item_id, &days.date(l.ms), l, now);
        }
        for b in held_bench {
            self.add_bench(item_id, b);
        }
        if let Some(item) = self.items.get_mut(item_id) {
            item.last_activity_ms = item.last_activity_ms.max(now);
            // A session the panel assigned before it started has no folder
            // yet; its first event sets the repo instead.
            if item.repo_path.is_none() && !cwd.is_empty() {
                item.repo_path = git.toplevel(&cwd);
            }
        }
        true
    }

    /// The desk holding this issue's Linear project.
    /// A desk bound to a project takes that project's issues; one bound to a
    /// whole team (no project) takes the team's.
    pub fn desk_for_issue(&self, issue: &IssueRef) -> Option<String> {
        self.desks
            .values()
            .find(|d| d.linear.as_ref().is_some_and(|l| l.holds(issue)))
            .map(|d| d.id.clone())
    }

    /// The desk whose firmware repo holds this path.
    pub fn desk_for_path(&self, path: &str) -> Option<String> {
        self.desks
            .values()
            .find(|d| !path.is_empty() && is_ignored(path, &d.firmware_repos))
            .map(|d| d.id.clone())
    }

    pub fn desk_by_name_or_id(&self, s: &str) -> Option<&Desk> {
        let s = s.trim();
        self.desks.get(s).or_else(|| self.desks.values().find(|d| d.name.eq_ignore_ascii_case(s)))
    }

    /// Save a desk; the first desk ever made also takes every work item that
    /// has none — the cards that existed before desks did. Items whose issue
    /// sits in the desk's Linear project move onto it.
    pub fn put_desk(&mut self, mut desk: Desk, now: i64) -> String {
        if desk.id.is_empty() {
            desk.id = self.new_id("k");
            desk.created_ms = now;
        }
        let first = self.desks.is_empty();
        let id = desk.id.clone();
        let linear = desk.linear.clone();
        self.desks.insert(id.clone(), desk);
        for item in self.items.values_mut() {
            let in_project = linear.as_ref().is_some_and(|l| item.issue.as_ref().is_some_and(|i| l.holds(i)));
            if in_project || (first && item.desk_id.is_none()) {
                item.desk_id = Some(id.clone());
            }
        }
        id
    }

    /// The work item an entry belongs to: the card carrying the issue its
    /// note names.
    pub fn item_for_entry(&self, e: &LoggedEntry) -> Option<String> {
        e.issue_key.as_deref().and_then(|k| self.item_by_key(k)).map(|i| i.id.clone())
    }

    /// The desk an entry with no issue belongs to: the one billing its
    /// Harvest project.
    pub fn desk_for_entry(&self, e: &LoggedEntry) -> Option<String> {
        self.desks
            .values()
            .find(|d| d.harvest.as_ref().is_some_and(|h| h.project_id == e.project_id))
            .map(|d| d.id.clone())
    }

    /// Hours Harvest already holds for an item on a day — leaving out the
    /// entry this item's own tile created, which is the tile, not a gap.
    pub fn logged_hours(&self, item_id: &str, date: &str) -> f64 {
        let own = self.tiles.get(&format!("{item_id}@{date}")).and_then(|t| t.harvest_entry_id);
        self.harvest_logged
            .get(date)
            .into_iter()
            .flatten()
            .filter(|e| Some(e.id) != own && self.item_for_entry(e).as_deref() == Some(item_id))
            .map(|e| e.hours)
            .sum()
    }

    /// The entry an item's tile created, as Harvest last read it.
    pub fn own_entry(&self, item_id: &str, date: &str) -> Option<&LoggedEntry> {
        let own = self.tiles.get(&format!("{item_id}@{date}")).and_then(|t| t.harvest_entry_id)?;
        self.harvest_logged.get(date)?.iter().find(|e| e.id == own)
    }

    /// Every hour Harvest holds for an item on a day, its own tile's included.
    pub fn harvest_hours(&self, item_id: &str, date: &str) -> f64 {
        self.logged_hours(item_id, date) + self.own_entry(item_id, date).map_or(0.0, |e| e.hours)
    }

    /// An approved tile's continuation (decision 64): the time its sessions
    /// ran after the ✓, less any hours Harvest took on for the item since.
    /// `None` when that is under [`MORE_MIN_SECONDS`] and nobody typed its
    /// hours, as a covered open tile closes itself.
    pub fn more(&self, item_id: &str, date: &str) -> Option<More> {
        let t = self.tiles.get(&format!("{item_id}@{date}")).filter(|t| t.status == TileStatus::Approved)?;
        let seconds = (self.live_seconds(item_id, date) - t.final_seconds.unwrap_or(0)).max(0);
        let now = self.harvest_hours(item_id, date);
        // A tile sent before the mark was kept measures from what ✓ sent.
        let base = t.harvest_at_send.or(t.sent_hours.map(|s| s + self.logged_hours(item_id, date))).unwrap_or(now);
        let grew = (now - base).max(0.0);
        let computed = ((seconds as f64 / 3600.0 - grew).max(0.0) * 100.0).round() / 100.0;
        if t.more_hours.is_none() && computed * 3600.0 < MORE_MIN_SECONDS as f64 {
            return None;
        }
        let entry_hours = self.own_entry(item_id, date).map(|e| e.hours).or(t.sent_hours).unwrap_or(0.0);
        Some(More { seconds, computed_hours: computed, hours: t.more_hours.unwrap_or(computed), entry_hours })
    }

    /// A tile still waiting on a person: open, or approved with a
    /// continuation.
    pub fn waiting(&self, item_id: &str, date: &str) -> bool {
        self.tiles.get(&format!("{item_id}@{date}")).is_none_or(|t| t.status == TileStatus::Open)
            || self.more(item_id, date).is_some()
    }

    /// The effective Harvest target of a tile: its own, else its desk's.
    pub fn tile_target(&self, item_id: &str, date: &str) -> (Option<HarvestTarget>, bool) {
        if let Some(h) = self.tiles.get(&format!("{item_id}@{date}")).and_then(|t| t.harvest.clone()) {
            return (Some(h), false);
        }
        let desk = self.items.get(item_id).and_then(|i| i.desk_id.as_ref()).and_then(|d| self.desks.get(d));
        (desk.and_then(|d| d.harvest.clone()), true)
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
        let mut sess = Session::new(&sid, "", now);
        sess.item_id = Some(item_id.to_string());
        record_activity(&mut sess, now, days);
        self.sessions.insert(sid.clone(), sess);
        self.timer = Some(Timer { session_id: sid, item_id: item_id.to_string(), started_ms: now });
        if let Some(i) = self.items.get_mut(item_id) {
            i.last_activity_ms = now;
        }
        true
    }

    /// Bring a running timer's span up to `now`. A timer is never idle, so
    /// the 8-hour gap rule must not end it: the sync loop ticks it every 30
    /// seconds.
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
    /// for the caller to append to history, and whether anything at all was
    /// removed, so a pass that removed nothing leaves the state file alone.
    pub fn prune(&mut self, now: i64) -> (Vec<Tile>, bool) {
        let before = (self.sessions.len(), self.pending_launches.len(), self.mailbox.len(), self.harvest_logged.len());
        let mut old: Vec<String> = self
            .tiles
            .values()
            .filter(|t| t.status != TileStatus::Open && t.acted_ms.is_some_and(|a| now - a > ARCHIVE_AFTER_MS))
            .filter(|t| self.more(&t.item_id, &t.date).is_none())
            .map(|t| t.id.clone())
            .collect();
        // A folded tile's seconds are part of its target's until the target
        // freezes them, so it goes to history with the target, not before.
        let going = old.clone();
        old.retain(|id| {
            let t = &self.tiles[id];
            match (&t.status, &t.folded_into) {
                (TileStatus::Folded, Some(d)) => {
                    let target = format!("{}@{d}", t.item_id);
                    going.contains(&target) || !self.tiles.contains_key(&target)
                }
                _ => true,
            }
        });
        let archived: Vec<Tile> = old.iter().filter_map(|id| self.tiles.remove(id)).collect();
        // The tile goes to history; its lines stay on the item, which is
        // where the work's knowledge gathers.
        for t in archived.iter().filter(|t| !t.lines.is_empty()) {
            let before = self.notes_ms(&t.item_id);
            self.notes.entry(t.item_id.clone()).or_default().extend(t.lines.iter().cloned());
            self.notes_moved(&t.item_id, before, now);
        }
        let open: std::collections::BTreeSet<(String, String)> =
            self.tile_keys().into_iter().filter(|(i, d)| self.waiting(i, d)).collect();
        self.sessions.retain(|_, s| {
            now - s.last_ms < SESSION_TTL_MS
                || s.spans.iter().any(|sp| {
                    sp.item_id
                        .as_ref()
                        .is_some_and(|i| open.contains(&(i.clone(), sp.date.clone())))
                })
        });
        self.pending_launches.retain(|p| p.expires_ms > now);
        self.mailbox.retain(|m| m.expires_ms > now);
        if let Some(cutoff) = chrono::DateTime::from_timestamp_millis(now - 14 * 86_400_000) {
            let cutoff = cutoff.date_naive().to_string();
            self.harvest_logged.retain(|d, _| d.as_str() >= cutoff.as_str());
        }
        let after = (self.sessions.len(), self.pending_launches.len(), self.mailbox.len(), self.harvest_logged.len());
        let changed = !archived.is_empty() || after != before;
        (archived, changed)
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
        fn stamp(&self, ms: i64) -> String {
            chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%Y-%m-%d %H:%M").to_string()
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
        Session::new(id, "/w/repo", 0)
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
        s.held_lines.push(Line::new(D0 + H, "fix the race", "commit"));
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
        let (archived, _) = st.prune(D0 + 8 * DAY);
        assert_eq!(archived.len(), 1);
        assert!(st.tiles.is_empty());
    }

    #[test]
    fn time_after_a_send_is_a_continuation_less_what_harvest_took_on_since() {
        let mut st = State::default();
        let issue = IssueRef { key: "ABC-1".into(), ..Default::default() };
        let w = st.create_item("x", Some(issue), D0);
        let mut s = session("s");
        s.item_id = Some(w.clone());
        record_activity(&mut s, D0 + 9 * H, &UtcDays);
        record_activity(&mut s, D0 + 10 * H, &UtcDays);
        st.sessions.insert("s".into(), s);
        // ✓ sent typed hours over the hour captured, as a person may.
        let t = st.tile_mut(&w, "2026-10-22");
        t.status = TileStatus::Approved;
        t.final_seconds = Some(3600);
        t.sent_hours = Some(1.5);
        t.hours_override = Some(1.5);
        t.harvest_entry_id = Some(7);
        t.acted_ms = Some(D0 + 10 * H);
        let entry = |id, hours| LoggedEntry { id, hours, issue_key: Some("ABC-1".into()), ..Default::default() };
        st.harvest_logged.insert("2026-10-22".into(), vec![entry(7, 1.5)]);
        let on = |st: &mut State, ms| {
            let mut s = st.sessions.remove("s").unwrap();
            record_activity(&mut s, ms, &UtcDays);
            st.sessions.insert("s".into(), s);
        };
        assert_eq!(st.more(&w, "2026-10-22"), None);
        on(&mut st, D0 + 10 * H + 4 * 60_000);
        assert_eq!(st.more(&w, "2026-10-22"), None, "a session's last minutes after the click are not more work");
        on(&mut st, D0 + 12 * H);
        let m = st.more(&w, "2026-10-22").unwrap();
        assert_eq!((m.seconds, m.hours, m.entry_hours), (7200, 2.0, 1.5), "the time after the ✓, whatever the ✓ sent");
        assert!(st.waiting(&w, "2026-10-22"));
        assert_eq!(st.tile_seconds(&w, "2026-10-22"), 3600, "the approved record keeps what it sent");
        // A tile sent before the mark was kept: the entry raised in Harvest
        // since its ✓ still counts.
        st.harvest_logged.insert("2026-10-22".into(), vec![entry(7, 2.5)]);
        assert_eq!(st.more(&w, "2026-10-22").unwrap().hours, 1.0);
        st.harvest_logged.insert("2026-10-22".into(), vec![entry(7, 1.5)]);
        // An hour typed into Harvest since the ✓ is not proposed again.
        st.tile_mut(&w, "2026-10-22").harvest_at_send = Some(1.5);
        st.harvest_logged.insert("2026-10-22".into(), vec![entry(7, 1.5), entry(8, 0.5)]);
        assert_eq!(st.more(&w, "2026-10-22").unwrap().hours, 1.5);
        st.harvest_logged.insert("2026-10-22".into(), vec![entry(7, 3.5)]);
        assert_eq!(st.more(&w, "2026-10-22"), None, "the entry raised in Harvest covers it");
        st.tile_mut(&w, "2026-10-22").more_hours = Some(0.25);
        assert_eq!(st.more(&w, "2026-10-22").unwrap().hours, 0.25, "typed hours are the person's call");
        let (archived, _) = st.prune(D0 + 8 * DAY);
        assert!(archived.is_empty() && st.sessions.contains_key("s"), "a waiting continuation is neither archived nor loses its sessions");
        st.reopen_tiles_of_entry(7);
        assert_eq!(st.tiles[&format!("{w}@2026-10-22")].more_hours, None);
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
    fn the_first_desk_adopts_the_cards_that_came_before_and_issues_follow_their_project() {
        let mut st = State::default();
        let old = st.create_item("Before desks", None, D0);
        let a = st.put_desk(Desk { name: "A".into(), ..Default::default() }, D0);
        assert_eq!(st.items[&old].desk_id.as_deref(), Some(a.as_str()));
        let linear = LinearBinding { project_id: "p-b".into(), ..Default::default() };
        let b = st.put_desk(Desk { name: "B".into(), linear: Some(linear), firmware_repos: vec!["/w/b".into()], ..Default::default() }, D0);
        let issue = IssueRef { key: "ABC-1".into(), project_id: "p-b".into(), ..Default::default() };
        let w = st.create_item("x", Some(issue), D0);
        assert_eq!(st.items[&w].desk_id.as_deref(), Some(b.as_str()));
        assert_eq!(st.desk_for_path("/w/b/app/src").as_deref(), Some(b.as_str()));
        assert_eq!(st.desk_for_path("/w/bb"), None);
        assert_eq!(st.desk_by_name_or_id("b").map(|d| d.id.clone()), Some(b));
        let team = LinearBinding { team_id: "t-c".into(), ..Default::default() };
        let c = st.put_desk(Desk { name: "C".into(), linear: Some(team), ..Default::default() }, D0);
        let in_team = IssueRef { key: "C-1".into(), team_id: "t-c".into(), project_id: "any".into(), ..Default::default() };
        assert_eq!(st.desk_for_issue(&in_team), Some(c), "a desk bound to a whole team takes all its projects");
    }

    #[test]
    fn a_tile_falls_back_to_its_desks_harvest_target() {
        let mut st = State::default();
        let target = HarvestTarget { account_id: 1, project_id: 2, task_id: 3, label: "C / P / T".into() };
        let k = st.put_desk(Desk { name: "A".into(), harvest: Some(target.clone()), ..Default::default() }, D0);
        let w = st.create_item("x", None, D0);
        st.items.get_mut(&w).unwrap().desk_id = Some(k);
        assert_eq!(st.tile_target(&w, "2026-10-22"), (Some(target), true));
        let own = HarvestTarget { account_id: 1, project_id: 9, task_id: 3, label: "other".into() };
        st.tile_mut(&w, "2026-10-22").harvest = Some(own.clone());
        assert_eq!(st.tile_target(&w, "2026-10-22"), (Some(own), false));
    }

    #[test]
    fn logged_hours_count_entries_naming_the_items_issue_but_not_its_own_tiles_entry() {
        let mut st = State::default();
        let issue = IssueRef { key: "ABC-1".into(), ..Default::default() };
        let w = st.create_item("x", Some(issue), D0);
        let entry = |id, key: Option<&str>, hours| LoggedEntry { id, issue_key: key.map(str::to_string), hours, ..Default::default() };
        st.harvest_logged.insert("2026-10-22".into(), vec![entry(1, Some("ABC-1"), 1.5), entry(2, Some("ABC-1"), 0.5), entry(3, None, 2.0), entry(4, Some("ZZ-9"), 3.0)]);
        assert_eq!(st.logged_hours(&w, "2026-10-22"), 2.0);
        st.tile_mut(&w, "2026-10-22").harvest_entry_id = Some(2);
        assert_eq!(st.logged_hours(&w, "2026-10-22"), 1.5, "the tile's own entry is not a gap");
        assert_eq!(st.logged_hours(&w, "2026-10-21"), 0.0);
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
        assert_eq!(issue_keys_in("ABC-1 blocks abc-22; see ABC-1 and ABC-3x"), ["ABC-1", "ABC-22"]);
    }

    #[test]
    fn lines_from_before_notes_load_as_done_notes_with_ids_and_nothing_is_lost() {
        // The shape the state file had before notes: lines are {ms, text, source}.
        let old = serde_json::json!({
            "next_id": 4,
            "items": { "w1": { "id": "w1", "title": "Loader", "created_ms": D0, "last_activity_ms": D0 } },
            "sessions": { "s": { "id": "s", "cwd": "/w/r", "started_ms": D0, "last_ms": D0,
                "held_lines": [{ "ms": D0 + 3, "text": "held", "source": "commit" }] } },
            "tiles": { "w1@2026-10-22": { "id": "w1@2026-10-22", "item_id": "w1", "date": "2026-10-22",
                "lines": [{ "ms": D0 + 1, "text": "smp: retry", "source": "commit" }, { "ms": D0 + 2, "text": "flashed", "source": "agent" }] } },
        });
        let mut st: State = serde_json::from_value(old).unwrap();
        assert!(st.migrate());
        let lines = st.tiles["w1@2026-10-22"].lines.clone();
        assert_eq!(lines.iter().map(|l| (l.text.as_str(), l.kind.as_str())).collect::<Vec<_>>(), [("smp: retry", "done"), ("flashed", "done")]);
        assert_eq!((lines[0].id.as_str(), lines[1].id.as_str()), ("n5", "n6"));
        assert_eq!(st.sessions["s"].held_lines[0].id, "n7");
        assert_eq!(st.next_id, 7, "new ids never collide with the ones given");
        assert!(!st.migrate(), "a second load changes nothing");
        let again: State = serde_json::from_value(serde_json::to_value(&st).unwrap()).unwrap();
        assert_eq!(again.tiles["w1@2026-10-22"].lines, lines);
        assert!(!again.settings.chat_panel && again.mailbox.is_empty() && !again.memory_imported);
        assert_eq!(again.notes_ms("w1"), Some(D0 + 2), "with no change recorded, the newest note's time");
    }

    #[test]
    fn notes_are_one_stream_and_a_memory_files_versions_stay_on_the_item() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        st.add_line(&w, "2026-10-22", "built the loader", "agent", D0 + H);
        let mut f = Line::new(D0 + 2 * H, "the loader needs 432 KB", "agent");
        f.kind = "finding".into();
        st.add_note(&w, "2026-10-22", f);
        assert!(st.add_note(&w, "2026-10-22", Line::new(D0, "   ", "agent")).is_none());
        st.add_note(&w, "2026-10-22", Line::memory(D0 + 3 * H, "/m/a.md", "v1"));
        st.add_note(&w, "2026-10-22", Line::memory(D0 + 4 * H, "/m/a.md", "v1"));
        st.add_note(&w, "2026-10-22", Line::memory(D0 + 5 * H, "/m/a.md", "v2"));
        st.add_note(&w, "2026-10-23", Line::memory(D0 + DAY, "/m/b.md", "other"));
        let tile = &st.tiles[&format!("{w}@2026-10-22")];
        assert_eq!(tile.lines.len(), 2, "a memory file is no tile line");
        assert_eq!(st.notes[&w].len(), 3, "the same text twice is one version");
        let all: Vec<&str> = st.item_notes(&w).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(all, ["other", "v2", "v1", "the loader needs 432 KB", "built the loader"]);
        let latest: Vec<&str> = st.memory_latest(&w).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(latest, ["other", "v2"]);
        let day: Vec<(String, String)> = st.day_notes(&w, "2026-10-22", &UtcDays).into_iter().map(|l| (l.kind, l.text)).collect();
        assert_eq!(day, [("done".into(), "built the loader".into()), ("finding".into(), "the loader needs 432 KB".into()), ("memory".into(), "v2".into())]);
        for i in 0..MEMORY_VERSIONS_KEPT + 5 {
            st.add_note(&w, "2026-10-24", Line::memory(D0 + 2 * DAY + i as i64, "/m/c.md", &format!("c{i}")));
        }
        assert_eq!(st.notes[&w].iter().filter(|l| l.source_path.as_deref() == Some("/m/c.md")).count(), MEMORY_VERSIONS_KEPT);
        // An acted tile goes to history; its lines stay on the item.
        let t = st.tile_mut(&w, "2026-10-22");
        t.status = TileStatus::Approved;
        t.acted_ms = Some(D0);
        assert_eq!(st.prune(D0 + 8 * DAY).0.len(), 1);
        assert!(st.item_notes(&w).iter().any(|l| l.text == "built the loader"));
    }

    #[test]
    fn the_import_attaches_a_file_by_its_name_else_its_frontmatter_to_one_item_only_once() {
        let mut st = State::default();
        let issue = |k: &str| Some(IssueRef { key: k.into(), ..Default::default() });
        let a = st.create_item("a", issue("ABC-1"), D0);
        let b = st.create_item("b", issue("ABC-2"), D0);
        let file = |path: &str, text: &str| MemoryFile { path: path.into(), mtime_ms: D0, text: text.into() };
        let files = vec![
            file("/m/abc-1-state.md", "ABC-1 branch; blocked on ABC-2"),
            file("/m/abc2-driver.md", "---\nname: driver\n---\nmentions ABC-1"),
            file("/m/bring-up.md", "---\nname: bring-up\ndescription: \"ABC-2 then ABC-1\"\n---\nbody"),
            file("/m/notes.md", "---\nname: notes\n---\nnotes on ABC-2 and ABC-1"),
            file("/m/unrelated.md", "ZZZ-9 only"),
        ];
        assert_eq!(st.import_memories(&files, D0), 3);
        let paths = |w: &str| st.notes.get(w).map(|n| n.iter().map(|l| l.source_path.clone().unwrap()).collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(paths(&a), ["/m/abc-1-state.md"], "a name decides; a body's mention never does");
        assert_eq!(paths(&b), ["/m/abc2-driver.md", "/m/bring-up.md"], "abc2 in a name is ABC-2; a frontmatter's first key wins alone");
        assert!(st.memory_imported);
        assert_eq!(st.notes[&a][0].kind, "memory");
        assert_eq!(st.notes[&a][0].ms, D0, "dated by the file, so no other day's notes change");
        assert_eq!(st.import_memories(&files, D0), 0, "idempotent");
    }

    #[test]
    fn a_line_added_to_yesterdays_tile_this_morning_is_yesterdays_note() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        st.add_line(&w, "2026-10-22", "agent line", "agent", D0 + H);
        // Tiles stay open until acted on: the next morning, the user adds a
        // line to yesterday's tile.
        st.add_line(&w, "2026-10-22", "user line, next morning", "user", D0 + DAY + 9 * H);
        st.add_line(&w, "2026-10-23", "today's own line", "agent", D0 + DAY + 10 * H);
        st.add_note(&w, "", Line::memory(D0 + DAY + 11 * H, "/m/a.md", "today's memory"));
        let day = |d: &str| st.day_notes(&w, d, &UtcDays).into_iter().map(|l| l.text).collect::<Vec<_>>();
        assert_eq!(day("2026-10-22"), ["agent line", "user line, next morning"]);
        assert_eq!(day("2026-10-23"), ["today's own line", "today's memory"]);
    }

    #[test]
    fn an_archived_tiles_next_morning_line_stays_out_of_the_next_days_polish() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        st.add_line(&w, "2026-10-22", "agent line", "agent", D0 + H);
        st.add_line(&w, "2026-10-22", "user line, next morning", "user", D0 + DAY + 9 * H);
        st.add_line(&w, "2026-10-23", "today's own line", "agent", D0 + DAY + 10 * H);
        st.add_note(&w, "", Line::memory(D0 + DAY + 11 * H, "/m/a.md", "today's memory"));
        let t = st.tile_mut(&w, "2026-10-22");
        t.status = TileStatus::Approved;
        t.acted_ms = Some(D0 + DAY + 12 * H);
        // 10-23's tile is left open past the week 10-22's goes to history in.
        assert_eq!(st.prune(D0 + 9 * DAY).0.len(), 1);
        assert!(st.item_notes(&w).iter().any(|l| l.text == "user line, next morning"), "kept on the item");
        let day: Vec<String> = st.day_notes(&w, "2026-10-23", &UtcDays).into_iter().map(|l| l.text).collect();
        assert_eq!(day, ["today's own line", "today's memory"], "10-22's line is not 10-23's work");
    }

    #[test]
    fn a_second_start_work_before_the_window_takes_the_first_is_one_start() {
        let mut st = State::default();
        let start = |item: &str, folder: &str| MailboxEntry {
            id: String::new(), kind: "start".into(), item_id: Some(item.into()), session_id: None,
            folder: folder.into(), prompt: Some("Read ABC-1".into()), created_ms: 0, expires_ms: 0, taken_ms: None,
        };
        st.post_mail(start("w1", "/w/fw-abc-1"), D0);
        let second = st.post_mail(start("w1", "/w/fw-abc-1/"), D0 + 1);
        let other = st.post_mail(start("w2", "/w/fw-abc-1"), D0 + 2);
        let ids: Vec<&str> = st.mail_for("/w/fw-abc-1", D0 + 3).iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, [second.as_str(), other.as_str()]);
        // One already taken is the window's: a new start is a new chat.
        st.take_mail(&second, D0 + 4).unwrap();
        let third = st.post_mail(start("w1", "/w/fw-abc-1"), D0 + 5);
        assert!(st.take_mail(&second, D0 + 6) == Err(TakeError::Taken));
        assert_eq!(st.mail_for("/w/fw-abc-1", D0 + 6).len(), 2, "{third} and {other}");
    }

    #[test]
    fn the_import_does_not_bring_a_quiet_card_back() {
        let mut st = State::default();
        let issue = IssueRef { key: "ABC-1".into(), ..Default::default() };
        let w = st.create_item("quiet", Some(issue), D0 - 30 * DAY);
        let files = [MemoryFile { path: "/m/abc-1.md".into(), mtime_ms: D0, text: "ABC-1: branch".into() }];
        assert_eq!(st.import_memories(&files, D0), 1);
        assert_eq!(st.items[&w].last_activity_ms, D0 - 30 * DAY, "an old file is not this week's work");
    }

    #[test]
    fn a_mailbox_entry_is_for_its_folder_taken_once_and_expires_as_a_claim_does() {
        let mut st = State::default();
        let entry = |folder: &str| MailboxEntry {
            id: String::new(), kind: "focus".into(), item_id: None, session_id: Some("s".into()),
            folder: folder.into(), prompt: None, created_ms: 0, expires_ms: 0, taken_ms: None,
        };
        let m = st.post_mail(entry("/w/fw-abc-1/"), D0);
        let other = st.post_mail(entry("/w/fw"), D0);
        assert_eq!(st.mail_for("/w/fw-abc-1", D0 + 1).iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [m.as_str()]);
        assert!(st.mail_for("/w", D0 + 1).is_empty(), "only the window whose folder matches");
        assert_eq!(st.take_mail(&m, D0 + 2).unwrap().folder, "/w/fw-abc-1");
        assert_eq!(st.take_mail(&m, D0 + 3), Err(TakeError::Taken));
        assert!(st.mail_for("/w/fw-abc-1", D0 + 4).is_empty());
        assert_eq!(st.take_mail(&other, D0 + LAUNCH_CLAIM_MS), Err(TakeError::Expired));
        assert_eq!(st.take_mail("m404", D0), Err(TakeError::Unknown));
        assert!(st.prune(D0 + LAUNCH_CLAIM_MS + 1).1, "dropping expired mail is a change");
        assert!(st.mailbox.is_empty());
        assert!(!st.prune(D0 + LAUNCH_CLAIM_MS + 2).1, "and once gone, nothing is");
        assert_eq!(norm_folder("/"), "/");
    }

    #[test]
    fn held_notes_landing_on_assignment_move_the_items_notes_time() {
        let mut st = State::default();
        let w = st.create_item("x", None, D0);
        // B writes a line and a memory file before it has an item; A, already
        // on the item, writes a line after.
        let mut b = session("b");
        b.held_lines.push(Line::new(D0 + H, "held line", "agent"));
        b.held_lines.push(Line::memory(D0 + H, "/m/b.md", "held memory"));
        st.sessions.insert("b".into(), b);
        st.add_line(&w, "2026-10-22", "A's line", "agent", D0 + 2 * H);
        assert_eq!(st.notes_ms(&w), Some(D0 + 2 * H));
        st.assign_session("b", &w, D0 + 3 * H, &UtcDays, &FakeGit::default());
        assert_eq!(st.item_notes(&w).len(), 3);
        assert!(st.notes_ms(&w) >= Some(D0 + 3 * H), "B's notes landed now, though taken before A's: {:?}", st.notes_ms(&w));
        assert!(st.item_notes(&w).iter().any(|l| l.text == "held line" && l.ms == D0 + H), "a note keeps the time it was taken");
    }

    #[test]
    fn every_change_to_an_items_notes_moves_its_notes_time() {
        let mut st = State::default();
        let issue = IssueRef { key: "ABC-1".into(), ..Default::default() };
        let w = st.create_item("x", Some(issue), D0);
        assert_eq!(st.notes_ms(&w), None);
        // A line from desk_add_line, the tile or a commit, at its own time.
        st.add_line(&w, "2026-10-22", "built it", "agent", D0 + H);
        assert_eq!(st.notes_ms(&w), Some(D0 + H));
        st.add_line(&w, "2026-10-22", "and flashed it", "agent", D0 + H);
        assert_eq!(st.notes_ms(&w), Some(D0 + H + 1), "two changes in one millisecond are two moves");
        // A memory file's new version; the same text again is no change.
        st.add_note(&w, "2026-10-22", Line::memory(D0 + 2 * H, "/m/a.md", "v1"));
        assert_eq!(st.notes_ms(&w), Some(D0 + 2 * H));
        st.add_note(&w, "2026-10-22", Line::memory(D0 + 3 * H, "/m/a.md", "v1"));
        assert_eq!(st.notes_ms(&w), Some(D0 + 2 * H));
        // The import attaches last month's file now.
        let files = [MemoryFile { path: "/m/abc-1-old.md".into(), mtime_ms: D0 - 30 * DAY, text: "ABC-1: old".into() }];
        assert_eq!(st.import_memories(&files, D0 + 4 * H), 1);
        assert_eq!(st.notes_ms(&w), Some(D0 + 4 * H));
        // An archived tile's lines move onto the item.
        let t = st.tile_mut(&w, "2026-10-22");
        t.status = TileStatus::Approved;
        t.acted_ms = Some(D0 + 5 * H);
        assert_eq!(st.prune(D0 + 9 * DAY).0.len(), 1);
        assert_eq!(st.notes_ms(&w), Some(D0 + 9 * DAY));
    }
}
