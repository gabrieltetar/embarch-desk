//! The checks before an issue's window is told to end its chat (decision 77).
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

/// Every worktree in an issue's folder, and each failed check as one
/// blocker: uncommitted files (untracked ones too), and, for `done`, a
/// branch not on `origin/main` after a fetch. A symlink to a main clone is
/// not a worktree and is not checked. A folder that is not there has none.
pub fn check(dir: &Path, outcome: &str) -> Vec<Blocker> {
    let mut repos: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.path().join(".git").exists())
        .collect();
    repos.sort_by_key(|e| e.file_name());
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
}
