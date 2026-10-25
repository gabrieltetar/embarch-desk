//! The reference index (decision 56): every issue in a desk's Linear scope
//! and every PR in its GitHub repos, by key, title, state and link — what
//! `embarch-chat` mirrors so that a PR a conversation names by its subject
//! can be offered as the PR itself, and an issue or PR named in the timeline
//! becomes a link. A card holds only its own item's PRs; a Done issue that
//! never had a card was invisible until this.
//!
//! **A request never waits on the network**: the sync loop refreshes each
//! desk's index at most every five minutes (and on `POST /sync`), and
//! `GET /refs` answers from memory. The index is kept in `refs.json` beside
//! `state.json`, written only when it changes, so a restart serves the last
//! one at once — and a polling window's state file is never rewritten for it.
//!
//! `synced_ms` moves only when a refresh changes the index, as a card's
//! `notes_ms` does: `GET /view`'s `refs_ms` carries it, and a client re-reads
//! `/refs` only when it moves.

use crate::connectors::Account;
use crate::store::{Desk, LinearBinding};
use crate::{github, linear, paths};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The most recently updated issues a desk's index keeps.
pub const ISSUES_CAP: usize = 1000;
/// The most recently updated PRs it keeps per repo.
pub const PRS_PER_REPO: usize = 300;
/// How often the sync loop refreshes one desk's index.
pub const REFRESH_MS: i64 = 5 * 60_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RefIssue {
    pub key: String,
    pub title: String,
    /// The workflow state's name: `Done`, `In Progress`, `Canceled`.
    pub state: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RefPr {
    pub repo: String,
    pub number: u64,
    pub title: String,
    /// `open`, `closed` or `merged`, as a card's PRs say it.
    pub state: String,
    pub draft: bool,
    pub url: String,
    pub branch: String,
    /// The issue keys in the title or branch whose team the index knows.
    #[serde(default)]
    pub keys: Vec<String>,
}

/// What `GET /refs` answers, field for field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RefIndex {
    pub desk_id: Option<String>,
    /// When a refresh last changed the index; null before the first.
    pub synced_ms: Option<i64>,
    pub issues: Vec<RefIssue>,
    pub prs: Vec<RefPr>,
}

/// What one desk's index is drawn from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    pub linear: Option<LinearBinding>,
    pub repos: Vec<String>,
}

impl Scope {
    pub fn of(desk: &Desk) -> Self {
        Self { linear: desk.linear.clone(), repos: desk.github_repos.clone() }
    }

    /// The Linear half, as one comparable string.
    fn linear_key(&self) -> String {
        self.linear.as_ref().map(|b| format!("{}|{}|{}", b.account, b.project_id, b.team_id)).unwrap_or_default()
    }
}

/// One refresh's result, each part `None` when its fetch failed — that part
/// then keeps what the index already held.
#[derive(Debug, Default)]
pub struct Fetched {
    pub issues: Option<Vec<RefIssue>>,
    /// Per repo of the scope, in its order.
    pub prs: Vec<(String, Option<Vec<RefPr>>)>,
}

/// Pages until `cap` rows or the last page: `fetch` takes the cursor (none
/// for the first page) and returns a page's rows and the next cursor.
pub async fn take_pages<T, C, F, Fut>(cap: usize, mut fetch: F) -> Result<Vec<T>>
where
    F: FnMut(Option<C>) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, Option<C>)>>,
{
    let mut out = vec![];
    let mut cursor = None;
    while out.len() < cap {
        let (rows, next) = fetch(cursor.take()).await?;
        out.extend(rows);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    out.truncate(cap);
    Ok(out)
}

/// Fetch a desk's index from Linear and GitHub, through the connectors the
/// desk already uses.
pub async fn fetch(linear_accounts: &[Account], gh: &[Account], scope: &Scope) -> Fetched {
    let issues = match &scope.linear {
        None => Some(vec![]),
        Some(b) => match linear::scope_issues(linear_accounts, b, ISSUES_CAP).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("reference index: Linear: {e:#}");
                None
            }
        },
    };
    let mut prs = vec![];
    for repo in &scope.repos {
        let got = match github::repo_prs(gh, repo, PRS_PER_REPO).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("reference index: {repo}: {e:#}");
                None
            }
        };
        prs.push((repo.clone(), got));
    }
    Fetched { issues, prs }
}

/// The issue keys in a PR's title or branch, in order, once each — the
/// `ABC-123` shape on the boundaries `github::names_key` checks — kept when
/// their team prefix is one the index knows, so `UTF-8` or `SHA-256` in a
/// title is not taken for an issue.
pub fn pr_keys(title: &str, branch: &str, prefixes: &BTreeSet<String>) -> Vec<String> {
    let mut keys: Vec<String> = vec![];
    for k in crate::store::issue_keys_in(title).into_iter().chain(crate::store::issue_keys_in(branch)) {
        let known = k.split('-').next().is_some_and(|p| prefixes.contains(p));
        if known && !keys.contains(&k) && github::names_key(title, branch, &k) {
            keys.push(k);
        }
    }
    keys
}

/// The team prefixes an index knows: its issues' and its binding's.
fn prefixes(issues: &[RefIssue], scope: &Scope) -> BTreeSet<String> {
    let mut p: BTreeSet<String> = issues.iter().filter_map(|i| i.key.split('-').next().map(str::to_string)).collect();
    if let Some(k) = scope.linear.as_ref().map(|b| b.team_key.to_ascii_uppercase()).filter(|k| !k.is_empty()) {
        p.insert(k);
    }
    p
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    /// The scope the index was drawn from: a failed part keeps its rows
    /// only while its scope is unchanged.
    #[serde(default)]
    linear_scope: String,
    #[serde(default)]
    repos: Vec<String>,
    index: RefIndex,
}

#[derive(Default)]
struct Inner {
    desks: BTreeMap<String, Entry>,
    /// When each desk's refresh last started.
    checked: HashMap<String, i64>,
}

/// Every desk's index, in memory and in `refs.json`.
pub struct Store {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl Store {
    /// Load the last index from `dir/refs.json`; a missing or unreadable
    /// file starts empty — it is a cache, the next refresh rebuilds it.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("refs.json");
        let desks = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!("{} unreadable, the index rebuilds: {e}", path.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self { path, inner: Mutex::new(Inner { desks, checked: HashMap::new() }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("refs lock")
    }

    /// The route's answer for a desk: its index, or an empty one before the
    /// first refresh.
    pub fn json(&self, desk_id: Option<&str>) -> Value {
        let inner = self.lock();
        let entry = desk_id.and_then(|d| inner.desks.get(d));
        let mut v = match entry {
            Some(e) => serde_json::to_value(&e.index),
            None => serde_json::to_value(RefIndex::default()),
        }
        .unwrap_or_default();
        v["desk_id"] = desk_id.into();
        v
    }

    pub fn synced_ms(&self, desk_id: &str) -> Option<i64> {
        self.lock().desks.get(desk_id).and_then(|e| e.index.synced_ms)
    }

    /// Is this desk's refresh due? A due one is claimed, so a `/sync`
    /// running beside the loop does not fetch it twice.
    pub fn claim(&self, desk_id: &str, now: i64) -> bool {
        let mut inner = self.lock();
        let due = inner.checked.get(desk_id).is_none_or(|t| now - t >= REFRESH_MS);
        if due {
            inner.checked.insert(desk_id.to_string(), now);
        }
        due
    }

    /// Make every desk due (`POST /sync`), or one (its desk was edited).
    pub fn forget_checked(&self, desk_id: Option<&str>) {
        let mut inner = self.lock();
        match desk_id {
            Some(d) => {
                inner.checked.remove(d);
            }
            None => inner.checked.clear(),
        }
    }

    /// Drop the indexes of desks that no longer exist.
    pub fn retain(&self, desk_ids: &[&str]) {
        let removed = {
            let mut inner = self.lock();
            let before = inner.desks.len();
            inner.desks.retain(|k, _| desk_ids.contains(&k.as_str()));
            inner.desks.len() != before
        };
        if removed {
            self.persist();
        }
    }

    /// Take a refresh's result. A part that failed keeps what the index
    /// held for the same scope; when every part failed nothing changes,
    /// unless the desk's scope did — then what no longer belongs goes.
    /// Returns whether the index changed.
    pub fn apply(&self, desk_id: &str, scope: &Scope, got: Fetched, now: i64) -> bool {
        let linear_scope = scope.linear_key();
        let changed = {
            let mut inner = self.lock();
            let parts = usize::from(scope.linear.is_some()) + scope.repos.len();
            let ok = usize::from(scope.linear.is_some() && got.issues.is_some()) + got.prs.iter().filter(|(_, p)| p.is_some()).count();
            let old = inner.desks.get(desk_id).cloned();
            let same_scope = old.as_ref().is_some_and(|o| o.linear_scope == linear_scope && o.repos == scope.repos);
            if parts > 0 && ok == 0 && (old.is_none() || same_scope) {
                return false;
            }
            let old = old.unwrap_or_default();
            let issues = match got.issues {
                Some(v) => v,
                None if old.linear_scope == linear_scope => old.index.issues.clone(),
                None => vec![],
            };
            let known = prefixes(&issues, scope);
            let mut prs = vec![];
            for (repo, rows) in got.prs {
                match rows {
                    Some(v) => prs.extend(v),
                    None => prs.extend(old.index.prs.iter().filter(|p| p.repo.eq_ignore_ascii_case(&repo)).cloned()),
                }
            }
            for p in &mut prs {
                p.keys = pr_keys(&p.title, &p.branch, &known);
            }
            let moved = old.index.synced_ms.is_none() || old.index.issues != issues || old.index.prs != prs;
            if !moved && same_scope {
                return false;
            }
            let synced_ms = if moved { Some(now) } else { old.index.synced_ms };
            let index = RefIndex { desk_id: Some(desk_id.to_string()), synced_ms, issues, prs };
            inner.desks.insert(desk_id.to_string(), Entry { linear_scope, repos: scope.repos.clone(), index });
            moved
        };
        self.persist();
        changed
    }

    fn persist(&self) {
        let bytes = {
            let inner = self.lock();
            serde_json::to_vec(&inner.desks)
        };
        let res = bytes.map_err(anyhow::Error::from).and_then(|b| paths::write_private(&self.path, &b));
        if let Err(e) = res {
            tracing::warn!("saving the reference index failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn issue(key: &str, state: &str) -> RefIssue {
        RefIssue { key: key.into(), title: format!("{key} title"), state: state.into(), url: format!("https://linear.app/ws/issue/{key}") }
    }

    fn pr(repo: &str, number: u64, title: &str, branch: &str) -> RefPr {
        RefPr {
            repo: repo.into(),
            number,
            title: title.into(),
            state: "merged".into(),
            draft: false,
            url: format!("https://github.com/{repo}/pull/{number}"),
            branch: branch.into(),
            keys: vec![],
        }
    }

    fn scope(repos: &[&str]) -> Scope {
        Scope {
            linear: Some(LinearBinding { account: "work".into(), team_id: "t1".into(), team_key: "ABC".into(), ..Default::default() }),
            repos: repos.iter().map(|r| r.to_string()).collect(),
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("desk-refs-{}", paths::random_hex(4).unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_prs_keys_come_from_its_title_or_branch_on_a_known_team() {
        let known: BTreeSet<String> = ["ABC".to_string(), "XY".to_string()].into();
        assert_eq!(pr_keys("Widget", "user/abc-12-widget", &known), ["ABC-12"]);
        assert_eq!(pr_keys("ABC-12 and xy-3: both", "abc-12-x", &known), ["ABC-12", "XY-3"]);
        assert_eq!(pr_keys("UTF-8 names, SHA-256 sums", "fix/v-2", &known), Vec::<String>::new());
        // A neighbour is its own key, never the shorter one.
        assert_eq!(pr_keys("ABC-120 other", "", &known), ["ABC-120"]);
        assert_eq!(pr_keys("XABC-15", "", &known), Vec::<String>::new());
    }

    #[tokio::test]
    async fn paging_stops_at_the_cap_or_the_last_page() {
        let calls = Cell::new(0);
        let endless = |c: Option<u32>| {
            calls.set(calls.get() + 1);
            let n = c.unwrap_or(1);
            async move { Ok(((0..100).map(|i| n * 1000 + i).collect::<Vec<u32>>(), Some(n + 1))) }
        };
        let got = take_pages(PRS_PER_REPO, endless).await.unwrap();
        assert_eq!((got.len(), calls.get()), (300, 3));
        assert_eq!(got[299], 3099);
        calls.set(0);
        let got = take_pages(ISSUES_CAP, |c: Option<u32>| {
            calls.set(calls.get() + 1);
            let n = c.unwrap_or(0);
            async move { Ok((vec![0u8; 250], (n < 9).then_some(n + 1))) }
        })
        .await
        .unwrap();
        assert_eq!((got.len(), calls.get()), (1000, 4), "1000 issues are four pages of 250");
        calls.set(0);
        let short = take_pages(ISSUES_CAP, |_: Option<u32>| {
            calls.set(calls.get() + 1);
            async { Ok((vec![1u8; 17], None)) }
        })
        .await
        .unwrap();
        assert_eq!((short.len(), calls.get()), (17, 1));
    }

    #[test]
    fn an_index_is_empty_before_its_first_refresh_and_names_its_desk() {
        let dir = scratch();
        let s = Store::open(&dir);
        let v = s.json(Some("k1"));
        assert_eq!(v, serde_json::json!({"desk_id": "k1", "synced_ms": null, "issues": [], "prs": []}));
        assert_eq!(s.synced_ms("k1"), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_refresh_takes_keys_and_moves_synced_ms_only_on_a_change() {
        let dir = scratch();
        let s = Store::open(&dir);
        let sc = scope(&["o/a", "o/b"]);
        let got = || Fetched {
            issues: Some(vec![issue("ABC-12", "Done"), issue("ABC-7", "Canceled")]),
            prs: vec![
                ("o/a".into(), Some(vec![pr("o/a", 7, "Widget", "user/abc-12-widget")])),
                ("o/b".into(), Some(vec![pr("o/b", 2, "UTF-8 fix", "main")])),
            ],
        };
        assert!(s.apply("k1", &sc, got(), 100));
        let v = s.json(Some("k1"));
        assert_eq!(v["synced_ms"], 100);
        assert_eq!(v["prs"][0]["keys"], serde_json::json!(["ABC-12"]));
        assert_eq!(v["prs"][1]["keys"], serde_json::json!([]));
        assert!(!s.apply("k1", &sc, got(), 200), "the same index is no change");
        assert_eq!(s.synced_ms("k1"), Some(100));
        let mut moved = got();
        moved.issues.as_mut().unwrap()[0].state = "In Review".into();
        assert!(s.apply("k1", &sc, moved, 300));
        assert_eq!(s.synced_ms("k1"), Some(300));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_failed_part_keeps_what_the_index_held_and_a_failed_refresh_changes_nothing() {
        let dir = scratch();
        let s = Store::open(&dir);
        let sc = scope(&["o/a", "o/b"]);
        let full = Fetched {
            issues: Some(vec![issue("ABC-1", "Done")]),
            prs: vec![
                ("o/a".into(), Some(vec![pr("o/a", 1, "ABC-1 one", "")])),
                ("o/b".into(), Some(vec![pr("o/b", 2, "two", "")])),
            ],
        };
        assert!(s.apply("k1", &sc, full, 100));
        let partial = Fetched { issues: None, prs: vec![("o/a".into(), Some(vec![])), ("o/b".into(), None)] };
        assert!(s.apply("k1", &sc, partial, 200), "o/a lost its PR");
        let v = s.json(Some("k1"));
        assert_eq!(v["issues"][0]["key"], "ABC-1");
        assert_eq!(v["prs"].as_array().unwrap().len(), 1);
        assert_eq!(v["prs"][0]["repo"], "o/b");
        let failed = Fetched { issues: None, prs: vec![("o/a".into(), None), ("o/b".into(), None)] };
        assert!(!s.apply("k1", &sc, failed, 300));
        assert_eq!(s.synced_ms("k1"), Some(200));
        // A desk moved to another team does not keep the old team's issues.
        let mut other = scope(&[]);
        other.linear.as_mut().unwrap().team_id = "t2".into();
        assert!(s.apply("k1", &other, Fetched { issues: None, prs: vec![] }, 400));
        assert_eq!(s.json(Some("k1"))["issues"], serde_json::json!([]));
        assert!(!s.apply("k9", &sc, Fetched { issues: None, prs: vec![("o/a".into(), None), ("o/b".into(), None)] }, 500));
        assert_eq!(s.json(Some("k9"))["synced_ms"], Value::Null, "a first refresh that failed is no sync");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_index_survives_a_restart_and_a_deleted_desks_index_goes() {
        let dir = scratch();
        let s = Store::open(&dir);
        let sc = scope(&["o/a"]);
        let got = Fetched {
            issues: Some(vec![issue("ABC-12", "Done")]),
            prs: vec![("o/a".into(), Some(vec![pr("o/a", 7, "Widget", "user/abc-12-widget")]))],
        };
        assert!(s.apply("k1", &sc, got, 100));
        assert!(s.apply("k2", &Scope::default(), Fetched::default(), 100), "a desk with nothing to index syncs empty");
        let again = Store::open(&dir);
        assert_eq!(again.json(Some("k1")), s.json(Some("k1")));
        assert_eq!(again.json(Some("k1"))["prs"][0]["keys"], serde_json::json!(["ABC-12"]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("refs.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        again.retain(&["k2"]);
        assert_eq!(Store::open(&dir).synced_ms("k1"), None);
        assert_eq!(Store::open(&dir).synced_ms("k2"), Some(100));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_due_refresh_is_claimed_once_per_five_minutes() {
        let s = Store::open(&std::env::temp_dir().join("desk-refs-never-written"));
        assert!(s.claim("k1", 0));
        assert!(!s.claim("k1", REFRESH_MS - 1));
        assert!(s.claim("k1", REFRESH_MS));
        s.forget_checked(Some("k1"));
        assert!(s.claim("k1", REFRESH_MS + 1), "an edited desk refreshes on the next tick");
        assert!(s.claim("k2", 0));
        s.forget_checked(None);
        assert!(s.claim("k1", REFRESH_MS + 2) && s.claim("k2", 1), "POST /sync makes every desk due");
    }
}
