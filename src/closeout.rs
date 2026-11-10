//! The checks before an issue's window is told to end its chat (decision 83).
//! One function, [`check`], so that what removes an issue's folder later runs
//! the very checks that let the window end.

use crate::store::Blocker;
use crate::worktree::{current_branch, git};
use std::path::Path;
use std::process::Command;

/// The branch a Done issue's work must be on, after a fetch.
const MAIN_REF: &str = "origin/main";

/// `done` or `canceled` for Linear's `completed` and `canceled` state types,
/// whatever the team calls its states; any other state is open.
pub fn outcome_of(state_type: &str) -> Option<&'static str> {
    match state_type {
        "completed" => Some("done"),
        "canceled" => Some("canceled"),
        _ => None,
    }
}

/// The worktrees in an issue's folder, by name: real folders with a `.git`.
/// A symlink to a main clone is not one (`DirEntry::file_type` does not
/// follow it), and a folder that is not there has none.
fn worktrees(dir: &Path) -> Vec<std::fs::DirEntry> {
    let mut repos: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.path().join(".git").exists())
        .collect();
    repos.sort_by_key(|e| e.file_name());
    repos
}

/// Every worktree in an issue's folder, and each failed check as one
/// blocker: uncommitted files (untracked ones too), and, for `done`, a
/// branch not on `origin/main` after a fetch. A symlink to a main clone is
/// not a worktree and is not checked. A folder that is not there has none.
pub fn check(dir: &Path, outcome: &str) -> Vec<Blocker> {
    let repos = worktrees(dir);
    let mut out = vec![];
    for e in repos {
        let repo = e.file_name().to_string_lossy().to_string();
        let mut block = |reason: String| out.push(Blocker { repo: repo.clone(), reason });
        match git(&e.path(), &["status", "--porcelain", "--untracked-files=all"]) {
            Ok(s) => {
                let n = s.lines().count();
                if n > 0 {
                    block(format!("{n} uncommitted file{} (untracked ones count)", if n == 1 { "" } else { "s" }));
                }
            }
            Err(err) => block(format!("could not read its status: {err:#}")),
        }
        if outcome == "done" {
            if let Some(reason) = unmerged(&e.path()) {
                block(reason);
            }
        }
    }
    out
}

/// Why the worktree's branch is not on `origin/main`, or `None` when it is.
fn unmerged(worktree: &Path) -> Option<String> {
    if let Err(err) = git(worktree, &["fetch", "origin"]) {
        return Some(format!("could not fetch origin: {err:#}"));
    }
    let branch = match current_branch(worktree) {
        Ok(b) => b,
        Err(_) => return Some("on no branch, so nothing says its work is on origin/main".into()),
    };
    let run = Command::new("git").arg("-C").arg(worktree).args(["merge-base", "--is-ancestor", &branch, MAIN_REF]).output();
    match run.map(|o| (o.status.code(), o)) {
        Ok((Some(0), _)) => None,
        Ok((Some(1), _)) => Some(format!("{branch} is not on {MAIN_REF}")),
        Ok((_, o)) => Some(format!("could not compare {branch} with {MAIN_REF}: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(err) => Some(format!("could not run git: {err}")),
    }
}

/// The remote a Canceled issue's branches are deleted from.
const REMOTE: &str = "origin";

/// Branches no removal deletes, whatever a worktree has checked out.
const KEPT_BRANCHES: [&str; 2] = ["main", "master"];

/// What [`remove`] did.
#[derive(Debug, PartialEq)]
pub enum Removed {
    /// The checks failed again: nothing was removed.
    Blocked(Vec<Blocker>),
    /// The worktrees, their local branches and the folder are gone.
    Gone,
}

/// One worktree to remove: where it is, its main clone and its branch.
struct Planned {
    repo: String,
    worktree: std::path::PathBuf,
    clone: std::path::PathBuf,
    branch: String,
}

fn plan(dir: &Path) -> Result<Vec<Planned>, String> {
    let mut out = vec![];
    for e in worktrees(dir) {
        let repo = e.file_name().to_string_lossy().to_string();
        let worktree = e.path();
        if worktree.join(".git").is_dir() {
            return Err(format!("{repo} is a clone, not a worktree: left in place"));
        }
        let branch = current_branch(&worktree).map_err(|err| format!("{repo}: {err:#}"))?;
        if KEPT_BRANCHES.contains(&branch.as_str()) {
            return Err(format!("{repo} is on {branch}, which is never deleted"));
        }
        let common = git(&worktree, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map_err(|err| format!("{repo}: {err:#}"))?;
        let clone = Path::new(&common).parent().map(Path::to_path_buf).ok_or_else(|| format!("{repo}: no main clone found"))?;
        out.push(Planned { repo, worktree, clone, branch });
    }
    Ok(out)
}

/// Delete `branch` on the remote; a remote that does not have it is not an error.
fn delete_remote_branch(clone: &Path, branch: &str) -> Result<(), String> {
    let has = Command::new("git").arg("-C").arg(clone).args(["ls-remote", "--exit-code", "--heads", REMOTE, branch]).output().map_err(|e| e.to_string())?;
    match has.status.code() {
        Some(2) => Ok(()),
        Some(0) => git(clone, &["push", REMOTE, "--delete", branch]).map(|_| ()).map_err(|e| format!("{e:#}")),
        _ => Err(String::from_utf8_lossy(&has.stderr).trim().to_string()),
    }
}

/// Remove an issue's folder: the checks run again first (the chat may have
/// worked since they last passed), and a failure removes nothing. Then, for
/// a Canceled issue, each branch on the remote; then for each worktree
/// `git worktree remove` from its main clone and its local branch; then the
/// folder itself. `remove_dir_all` does not follow symlinks, so the sibling
/// links the folder holds go and their targets stay. Everything is planned
/// before anything changes, and the remote goes first, so a failure early
/// leaves the folder whole; an `Err` names what could not be done.
pub fn remove(dir: &Path, outcome: &str) -> Result<Removed, String> {
    let blockers = check(dir, outcome);
    if !blockers.is_empty() {
        return Ok(Removed::Blocked(blockers));
    }
    if dir.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("the folder is a symlink: left in place".into());
    }
    let planned = plan(dir)?;
    if outcome == "canceled" {
        for p in &planned {
            delete_remote_branch(&p.clone, &p.branch).map_err(|e| format!("{}: could not delete {REMOTE}/{}: {e}", p.repo, p.branch))?;
        }
    }
    for p in &planned {
        git(&p.clone, &["worktree", "remove", &p.worktree.to_string_lossy()]).map_err(|e| format!("{}: {e:#}", p.repo))?;
        git(&p.clone, &["branch", "-D", &p.branch]).map_err(|e| format!("{}: {e:#}", p.repo))?;
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(Removed::Gone),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Removed::Gone),
        Err(e) => Err(format!("could not remove the folder: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    /// A folder holding one worktree, `fw`, on branch `emb-1`, whose origin
    /// has `main`. Returns (folder, scratch root).
    fn folder() -> (std::path::PathBuf, std::path::PathBuf) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!("embarch-closeout-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&root);
        let origin = root.join("origin.git");
        let clone = root.join("clone");
        std::fs::create_dir_all(&clone).unwrap();
        sh(&root, &["init", "--bare", "-b", "main", origin.to_str().unwrap()]);
        sh(&clone, &["init", "-b", "main"]);
        sh(&clone, &["config", "user.email", "t@example.com"]);
        sh(&clone, &["config", "user.name", "t"]);
        sh(&clone, &["commit", "--allow-empty", "-m", "first"]);
        sh(&clone, &["remote", "add", "origin", origin.to_str().unwrap()]);
        sh(&clone, &["push", "origin", "main"]);
        let dir = root.join("issue");
        std::fs::create_dir_all(&dir).unwrap();
        sh(&clone, &["worktree", "add", "-b", "emb-1", dir.join("fw").to_str().unwrap()]);
        (dir, root)
    }

    fn reasons(b: &[Blocker]) -> Vec<(String, String)> {
        b.iter().map(|b| (b.repo.clone(), b.reason.clone())).collect()
    }

    #[test]
    fn state_types_not_names_say_done_and_canceled() {
        assert_eq!(outcome_of("completed"), Some("done"));
        assert_eq!(outcome_of("canceled"), Some("canceled"));
        assert_eq!((outcome_of("started"), outcome_of("backlog"), outcome_of("")), (None, None, None));
    }

    #[test]
    fn a_clean_branch_on_origin_main_passes_and_a_missing_folder_has_no_blockers() {
        let (dir, root) = folder();
        assert!(check(&dir, "done").is_empty(), "{:?}", check(&dir, "done"));
        assert!(check(&root.join("nowhere"), "done").is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn uncommitted_and_untracked_files_each_block_either_outcome() {
        let (dir, root) = folder();
        std::fs::write(dir.join("fw").join("new.txt"), "x").unwrap();
        for outcome in ["done", "canceled"] {
            let b = check(&dir, outcome);
            assert_eq!(b.len(), 1, "{outcome}: {b:?}");
            assert_eq!(b[0].repo, "fw");
            assert!(b[0].reason.contains("1 uncommitted file"), "{}", b[0].reason);
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn only_done_needs_the_branch_on_origin_main() {
        let (dir, root) = folder();
        sh(&dir.join("fw"), &["config", "user.email", "t@example.com"]);
        sh(&dir.join("fw"), &["config", "user.name", "t"]);
        sh(&dir.join("fw"), &["commit", "--allow-empty", "-m", "work"]);
        let done = check(&dir, "done");
        assert_eq!(reasons(&done), [("fw".to_string(), "emb-1 is not on origin/main".to_string())]);
        assert!(check(&dir, "canceled").is_empty(), "a canceled issue's work need not have landed");
        // Landed: pushed to origin's main.
        sh(&dir.join("fw"), &["push", "origin", "emb-1:main"]);
        assert!(check(&dir, "done").is_empty(), "{:?}", check(&dir, "done"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_symlink_to_a_main_clone_is_not_checked() {
        let (dir, root) = folder();
        std::fs::write(root.join("clone").join("dirty.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("clone"), dir.join("clone")).unwrap();
        assert!(check(&dir, "canceled").is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    fn remote_has(root: &Path, branch: &str) -> bool {
        let o = Command::new("git").arg("-C").arg(root.join("clone")).args(["ls-remote", "--heads", "origin", branch]).output().unwrap();
        !String::from_utf8_lossy(&o.stdout).trim().is_empty()
    }

    fn local_has(root: &Path, branch: &str) -> bool {
        Command::new("git").arg("-C").arg(root.join("clone")).args(["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}")]).output().unwrap().status.success()
    }

    #[test]
    fn removal_takes_worktree_branch_and_folder_and_leaves_a_symlinks_target_alone() {
        let (dir, root) = folder();
        let sibling = root.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("keep.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&sibling, dir.join("sibling")).unwrap();
        std::os::unix::fs::symlink(root.join("clone"), dir.join("clone")).unwrap();
        // Landed: the same commit is on origin's main.
        assert_eq!(remove(&dir, "done"), Ok(Removed::Gone));
        assert!(!dir.exists());
        assert!(sibling.join("keep.txt").exists(), "a symlink's target is not followed");
        assert!(root.join("clone").join(".git").is_dir());
        assert!(!local_has(&root, "emb-1"), "the local branch is deleted");
        assert!(git(&root.join("clone"), &["worktree", "list"]).unwrap().lines().count() == 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn canceled_deletes_the_branch_on_the_remote_and_a_remote_without_it_is_no_error() {
        let (dir, root) = folder();
        sh(&dir.join("fw"), &["push", "origin", "emb-1"]);
        assert!(remote_has(&root, "emb-1"));
        assert_eq!(remove(&dir, "canceled"), Ok(Removed::Gone));
        assert!(!remote_has(&root, "emb-1"), "a canceled issue's remote branch goes");
        std::fs::remove_dir_all(&root).ok();
        // The remote never had it.
        let (dir, root) = folder();
        assert!(!remote_has(&root, "emb-1"));
        assert_eq!(remove(&dir, "canceled"), Ok(Removed::Gone));
        assert!(!dir.exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn done_keeps_the_remote_branch() {
        let (dir, root) = folder();
        sh(&dir.join("fw"), &["push", "origin", "emb-1"]);
        assert_eq!(remove(&dir, "done"), Ok(Removed::Gone));
        assert!(remote_has(&root, "emb-1"), "only a canceled issue deletes the remote branch");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn uncommitted_files_at_removal_time_remove_nothing() {
        let (dir, root) = folder();
        sh(&dir.join("fw"), &["push", "origin", "emb-1"]);
        std::fs::write(dir.join("fw").join("new.txt"), "x").unwrap();
        let Ok(Removed::Blocked(b)) = remove(&dir, "canceled") else { panic!("expected blockers") };
        assert_eq!((b[0].repo.as_str(), b[0].reason.contains("uncommitted")), ("fw", true));
        assert!(dir.join("fw").join("new.txt").exists() && local_has(&root, "emb-1") && remote_has(&root, "emb-1"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_folder_that_is_a_clone_or_on_main_is_left_whole() {
        let (dir, root) = folder();
        sh(&dir.join("fw"), &["checkout", "--detach"]);
        assert!(remove(&dir, "canceled").is_err(), "no branch to delete");
        assert!(dir.join("fw").exists());
        sh(&dir.join("fw"), &["checkout", "-B", "master"]);
        assert!(remove(&dir, "canceled").unwrap_err().contains("never deleted"));
        assert!(dir.join("fw").exists());
        std::fs::remove_dir_all(root).ok();
    }
}
