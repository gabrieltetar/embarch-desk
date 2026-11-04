//! Start work for a suite: repos that sit side by side under one folder and
//! reach each other by sibling path (decision 70). One worktree beside the
//! main clone would build against main's siblings, so an issue gets a folder
//! of its own instead:
//!
//! ```text
//! <root>/.dev-worktrees/<key>/
//!   <code repo>/   worktree on the issue's branch
//!   <docs repo>/   worktree on the same branch
//!   <other repo>   symlink to its main clone, until the work needs to change it
//! ```
//!
//! A desk is a suite desk when `EMBARCH_DESK_SUITE_ROOT` is set in its
//! environment. With none, Start work is the single worktree it always was.

use crate::worktree::{self, Prepared};
use anyhow::{bail, ensure, Context, Result};
use std::path::{Path, PathBuf};

/// Under the suite's root, so that sibling paths and any rule keyed on the
/// root's path hold for the work done there.
pub const WORKTREES: &str = ".dev-worktrees";

#[derive(Debug, Clone, PartialEq)]
pub struct Suite {
    pub root: PathBuf,
    /// The repo every issue also gets a worktree of, because the docs move
    /// with the code they describe.
    pub docs_repo: Option<String>,
}

/// One path component a repo or issue key can safely become: no separators,
/// and neither `..` nor a dotted folder such as the worktrees' own.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\'])
}

impl Suite {
    /// The suite this desk's environment names, if any.
    pub fn from_env() -> Option<Suite> {
        let root = std::env::var("EMBARCH_DESK_SUITE_ROOT").ok().filter(|r| !r.trim().is_empty())?;
        let docs_repo = std::env::var("EMBARCH_DESK_SUITE_DOCS").ok().filter(|d| !d.trim().is_empty());
        Some(Suite { root: PathBuf::from(root), docs_repo })
    }

    /// The issue's folder. Lowercase, as the single worktree's name is.
    pub fn issue_dir(&self, issue_key: &str) -> Result<PathBuf> {
        ensure!(is_plain_name(issue_key), "{issue_key:?} cannot name a folder");
        Ok(self.root.join(WORKTREES).join(issue_key.to_ascii_lowercase()))
    }

    /// The main clone of a repo in the suite. A worktree's `.git` is a file,
    /// so only a real clone answers.
    fn clone_of(&self, repo: &str) -> Result<PathBuf> {
        ensure!(is_plain_name(repo), "{repo:?} is not a repo name");
        let clone = self.root.join(repo);
        ensure!(clone.join(".git").is_dir(), "no clone of {repo} at {}", clone.display());
        Ok(clone)
    }

    /// The code repo of an issue: its Project's name is the clone's folder.
    pub fn repo_for_project(&self, project_name: &str) -> Result<String> {
        ensure!(!project_name.is_empty(), "the issue has no Project: put it in the Project of the repo doing most of the change");
        self.clone_of(project_name).with_context(|| format!("Project {project_name:?} names no repo in {}", self.root.display()))?;
        Ok(project_name.to_string())
    }

    /// Make (or find) an issue's folder. The answer's path is the code
    /// repo's worktree, which is what a window opens.
    pub fn start(&self, issue_key: &str, code_repo: &str, branch: &str) -> Result<Prepared> {
        let mut prepared = self.add_repo(issue_key, code_repo, branch)?;
        prepared.how = format!("{code_repo}: {}", prepared.how);
        if let Some(docs) = self.docs_repo.as_deref().filter(|d| *d != code_repo) {
            let d = self.add_repo(issue_key, docs, branch)?;
            prepared.how = format!("{}; {docs}: {}", prepared.how, d.how);
            prepared.warnings.extend(d.warnings);
        }
        Ok(prepared)
    }

    /// Give a repo a worktree in the issue's folder, replacing its symlink.
    /// Links are refreshed either way, which also puts back a link that a
    /// failed `git worktree add` lost, and picks up a clone added since.
    pub fn add_repo(&self, issue_key: &str, repo: &str, branch: &str) -> Result<Prepared> {
        let dir = self.issue_dir(issue_key)?;
        let clone = self.clone_of(repo)?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // A worktree folder removed by hand must not count as "checked out".
        worktree::prune(&clone)?;
        let target = dir.join(repo);
        let prepared = worktree::prepare_at(&clone, &target, branch);
        let linked = self.link_siblings(&dir);
        let prepared = prepared?;
        linked?;
        // The folder is real and the last part is not resolved, so a symlink
        // to a main clone that happens to be on the branch does not pass.
        let expected = dir.canonicalize()?.join(repo);
        if prepared.path.canonicalize()? != expected {
            bail!("{branch} is checked out at {}: finish there, or remove that worktree", prepared.path.display());
        }
        worktree::set_auto_mode(&target)?;
        worktree::add_directory(&target, &dir)?;
        Ok(prepared)
    }

    /// A symlink for every clone in the suite the folder has nothing for, so
    /// `../<repo>` resolves as it does from a main clone.
    fn link_siblings(&self, dir: &Path) -> Result<()> {
        for entry in std::fs::read_dir(&self.root).with_context(|| format!("reading {}", self.root.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|n| is_plain_name(n)) else { continue };
            let link = dir.join(name);
            if entry.path().join(".git").is_dir() && link.symlink_metadata().is_err() {
                symlink(&entry.path(), &link).with_context(|| format!("linking {}", link.display()))?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a suite's links need a Unix host"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    fn head_branch(dir: &Path) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--abbrev-ref", "HEAD"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A suite of three clones, each with a bare origin, and a stray folder.
    fn fixture() -> (Suite, PathBuf) {
        let base = std::env::temp_dir().join(format!("desk-suite-{}", crate::paths::random_hex(4).unwrap()));
        let root = base.join("suite");
        std::fs::create_dir_all(root.join("not-a-repo")).unwrap();
        for repo in ["app", "lib", "docs"] {
            let origin = base.join(format!("{repo}.git"));
            let seed = base.join(format!("{repo}-seed"));
            std::fs::create_dir_all(&seed).unwrap();
            sh(&base, &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
            sh(&seed, &["init", "--quiet", "-b", "main"]);
            sh(&seed, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--quiet", "--allow-empty", "-m", "root"]);
            sh(&seed, &["remote", "add", "origin", origin.to_str().unwrap()]);
            sh(&seed, &["push", "--quiet", "origin", "main"]);
            sh(&base, &["clone", "--quiet", origin.to_str().unwrap(), root.join(repo).to_str().unwrap()]);
        }
        (Suite { root, docs_repo: Some("docs".into()) }, base)
    }

    #[test]
    fn start_makes_worktrees_for_the_code_repo_and_the_docs_and_links_the_rest() {
        let (suite, base) = fixture();
        let p = suite.start("ABC-1", "app", "me/abc-1-x").unwrap();
        let dir = suite.root.join(".dev-worktrees/abc-1");
        assert_eq!(p.path, dir.join("app"), "a window opens the code repo");
        for repo in ["app", "docs"] {
            assert!(!dir.join(repo).symlink_metadata().unwrap().file_type().is_symlink(), "{repo} is a worktree");
            assert_eq!(head_branch(&dir.join(repo)), "me/abc-1-x");
        }
        assert_eq!(std::fs::read_link(dir.join("lib")).unwrap(), suite.root.join("lib"));
        assert!(!dir.join("not-a-repo").exists(), "only clones are linked");
        assert!(!dir.join(".dev-worktrees").exists());
        assert_eq!(head_branch(&suite.root.join("app")), "main", "the main clone stays where it was");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn the_worktrees_are_in_auto_mode_and_reach_the_issue_folder() {
        let (suite, base) = fixture();
        suite.start("ABC-1", "app", "me/abc-1-x").unwrap();
        let dir = suite.root.join(".dev-worktrees/abc-1");
        let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("app/.claude/settings.local.json")).unwrap()).unwrap();
        assert_eq!(s["permissions"]["defaultMode"], "auto");
        assert_eq!(s["permissions"]["additionalDirectories"], serde_json::json!([dir.to_str().unwrap()]));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn adding_a_repo_replaces_its_symlink_and_leaves_the_main_clone_alone() {
        let (suite, base) = fixture();
        suite.start("ABC-1", "app", "me/abc-1-x").unwrap();
        let dir = suite.root.join(".dev-worktrees/abc-1");
        std::fs::write(suite.root.join("lib/untouched"), "kept").unwrap();
        let p = suite.add_repo("ABC-1", "lib", "me/abc-1-x").unwrap();
        assert!(!dir.join("lib").symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!((p.path, head_branch(&dir.join("lib"))), (dir.join("lib"), "me/abc-1-x".to_string()));
        assert_eq!(std::fs::read_to_string(suite.root.join("lib/untouched")).unwrap(), "kept");
        assert!(!dir.join("lib/untouched").exists(), "the worktree comes from git, not the link");
        assert_eq!(suite.add_repo("ABC-1", "lib", "me/abc-1-x").unwrap().how, "reused");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_second_start_reuses_the_folder_and_a_new_clone_gets_linked() {
        let (suite, base) = fixture();
        suite.start("ABC-1", "app", "me/abc-1-x").unwrap();
        sh(&suite.root, &["clone", "--quiet", base.join("lib.git").to_str().unwrap(), "later"]);
        let again = suite.start("ABC-1", "app", "me/abc-1-x").unwrap();
        assert_eq!(again.how, "app: reused; docs: reused");
        assert!(suite.root.join(".dev-worktrees/abc-1/later").symlink_metadata().unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_docs_issue_is_one_worktree() {
        let (suite, base) = fixture();
        assert_eq!(suite.start("ABC-1", "docs", "me/abc-1-x").unwrap().how.matches("docs:").count(), 1);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_branch_on_a_main_clone_is_refused_rather_than_used_through_its_link() {
        let (suite, base) = fixture();
        sh(&suite.root.join("app"), &["switch", "--quiet", "-c", "me/abc-1-x"]);
        let err = suite.add_repo("ABC-1", "app", "me/abc-1-x").unwrap_err().to_string();
        assert!(err.contains("is checked out at"), "{err}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn something_real_in_the_way_is_never_touched() {
        let (suite, base) = fixture();
        let dir = suite.root.join(".dev-worktrees/abc-1");
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(dir.join("app/mine"), "work").unwrap();
        assert!(suite.add_repo("ABC-1", "app", "me/abc-1-x").unwrap_err().to_string().contains("already exists"));
        assert_eq!(std::fs::read_to_string(dir.join("app/mine")).unwrap(), "work");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_project_must_name_a_clone_and_a_name_cannot_leave_the_suite() {
        let (suite, base) = fixture();
        assert_eq!(suite.repo_for_project("app").unwrap(), "app");
        assert!(suite.repo_for_project("").unwrap_err().to_string().contains("no Project"));
        assert!(format!("{:#}", suite.repo_for_project("nope").unwrap_err()).contains("names no repo"));
        assert!(suite.repo_for_project("not-a-repo").is_err());
        for bad in ["..", "../app", "app/..", ".dev-worktrees", "/etc"] {
            assert!(suite.add_repo("ABC-1", bad, "b").is_err(), "{bad}");
        }
        assert!(suite.issue_dir("../x").is_err());
        assert!(suite.issue_dir("..").is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_failed_add_puts_the_link_back() {
        let (suite, base) = fixture();
        suite.start("ABC-1", "app", "me/abc-1-x").unwrap();
        let dir = suite.root.join(".dev-worktrees/abc-1");
        // A name git refuses, so the add fails after the link has gone.
        assert!(suite.add_repo("ABC-1", "lib", "bad..name").is_err());
        assert_eq!(std::fs::read_link(dir.join("lib")).unwrap(), suite.root.join("lib"));
        std::fs::remove_dir_all(base).unwrap();
    }
}
