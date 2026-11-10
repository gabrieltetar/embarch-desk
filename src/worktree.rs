//! Start work's git half (`embarch-desk-sessions-proposal.md`): a worktree
//! beside the main clone, on Linear's branch for the issue — checked out if
//! it exists locally or on the remote, else branched from the fresh remote
//! default branch. A branch already checked out in some worktree is that
//! worktree, reused rather than refused. A suite puts the worktree somewhere
//! else (`suite.rs`), and everything below `prepare` is shared with it.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `<project folder>/<repo>-<issue key>`: the owner's per-feature clones'
/// own pattern.
pub fn worktree_path(main_clone: &Path, issue_key: &str) -> PathBuf {
    let name = main_clone.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    main_clone.with_file_name(format!("{name}-{}", issue_key.to_ascii_lowercase()))
}

/// The worktree that has `branch` checked out, if any.
fn checked_out_at(main_clone: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let list = git(main_clone, &["worktree", "list", "--porcelain"])?;
    let want = format!("branch refs/heads/{branch}");
    let mut current: Option<PathBuf> = None;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(p));
        } else if line == want {
            return Ok(current);
        }
    }
    Ok(None)
}

fn has_ref(dir: &Path, r: &str) -> bool {
    git(dir, &["rev-parse", "--verify", "--quiet", r]).is_ok()
}

#[derive(Debug)]
pub struct Prepared {
    pub path: PathBuf,
    pub branch: String,
    /// `reused`, `local branch`, `remote branch` or `new from <base>`.
    pub how: String,
    pub warnings: Vec<String>,
}

/// Make (or find) the issue's worktree. Fetching is attempted and its
/// failure is a warning, not a stop: the local refs still give a worktree.
pub fn prepare(main_clone: &Path, issue_key: &str, branch: &str) -> Result<Prepared> {
    prepare_at(main_clone, &worktree_path(main_clone, issue_key), branch)
}

/// The same at a given path. A symlink there gives way to the worktree, its
/// target untouched: anything else there is someone's work.
pub fn prepare_at(main_clone: &Path, path: &Path, branch: &str) -> Result<Prepared> {
    let mut warnings = vec![];
    if !main_clone.join(".git").exists() {
        bail!("{} is not a git clone", main_clone.display());
    }
    if let Some(p) = checked_out_at(main_clone, branch)? {
        return Ok(Prepared { path: p, branch: branch.into(), how: "reused".into(), warnings });
    }
    let existing = path.symlink_metadata().ok();
    let is_link = existing.as_ref().is_some_and(|m| m.file_type().is_symlink());
    if existing.is_some() && !is_link {
        // A folder of that name that is not on the branch is someone's work:
        // never touched, named instead.
        bail!("{} already exists and is not on {branch}", path.display());
    }
    let remote = remote_of(main_clone);
    if let Err(e) = git(main_clone, &["fetch", "--quiet", &remote]) {
        warnings.push(format!("could not fetch {remote}, using local refs: {e:#}"));
    }
    if is_link {
        std::fs::remove_file(path).with_context(|| format!("removing the link at {}", path.display()))?;
    }
    let p = path.to_string_lossy().into_owned();
    let how = if has_ref(main_clone, &format!("refs/heads/{branch}")) {
        git(main_clone, &["worktree", "add", &p, branch])?;
        "local branch".to_string()
    } else if has_ref(main_clone, &format!("refs/remotes/{remote}/{branch}")) {
        git(main_clone, &["worktree", "add", "--track", "-b", branch, &p, &format!("{remote}/{branch}")])?;
        "remote branch".to_string()
    } else {
        let base = git(main_clone, &["symbolic-ref", "--quiet", "--short", &format!("refs/remotes/{remote}/HEAD")])
            .ok()
            .filter(|b| !b.is_empty())
            .or_else(|| ["main", "master"].iter().map(|b| format!("{remote}/{b}")).find(|b| has_ref(main_clone, b)))
            .with_context(|| format!("no remote default branch to start from ({remote}/HEAD, {remote}/main or {remote}/master)"))?;
        git(main_clone, &["worktree", "add", "--no-track", "-b", branch, &p, &base])?;
        format!("new from {base}")
    };
    Ok(Prepared { path: path.to_path_buf(), branch: branch.into(), how, warnings })
}

/// Forget worktrees whose folders are gone.
pub fn prune(main_clone: &Path) -> Result<()> {
    git(main_clone, &["worktree", "prune"]).map(|_| ())
}

/// The branch a worktree is on; a detached one has none to share.
pub fn current_branch(worktree: &Path) -> Result<String> {
    let branch = git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        bail!("{} is on no branch", worktree.display());
    }
    Ok(branch)
}

/// The remote a clone works against: the one its checked-out branch tracks,
/// else its only remote, else `origin`. A clone whose remote is named after
/// its organisation, not `origin`, is common.
fn remote_of(main_clone: &Path) -> String {
    let tracked = git(main_clone, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .and_then(|b| git(main_clone, &["config", &format!("branch.{b}.remote")]).ok())
        .filter(|r| !r.is_empty() && r != ".");
    if let Some(r) = tracked {
        return r;
    }
    let remotes: Vec<String> = git(main_clone, &["remote"]).map(|o| o.lines().map(str::to_string).collect()).unwrap_or_default();
    match remotes.as_slice() {
        [only] => only.clone(),
        _ => "origin".into(),
    }
}

/// Sessions the desk opens run in auto mode: a local settings file in the
/// worktree, kept out of git through the repo's shared `info/exclude`.
pub fn set_auto_mode(worktree: &Path) -> Result<()> {
    edit_permissions(worktree, |perms| {
        perms.entry("defaultMode").or_insert_with(|| serde_json::json!("auto"));
    })
}

/// Let a session in the worktree reach `dir` as well, through the same file.
pub fn add_directory(worktree: &Path, dir: &Path) -> Result<()> {
    let dir = dir.to_string_lossy().into_owned();
    edit_permissions(worktree, |perms| {
        let list = perms.entry("additionalDirectories").or_insert_with(|| serde_json::json!([]));
        if let Some(list) = list.as_array_mut() {
            if !list.iter().any(|d| d.as_str() == Some(dir.as_str())) {
                list.push(serde_json::json!(dir));
            }
        }
    })
}

fn edit_permissions(worktree: &Path, edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) -> Result<()> {
    let dir = worktree.join(".claude");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("settings.local.json");
    let mut v: serde_json::Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    let perms = v.as_object_mut().expect("object").entry("permissions").or_insert_with(|| serde_json::json!({}));
    if let Some(p) = perms.as_object_mut() {
        edit(p);
    }
    std::fs::write(&file, serde_json::to_string_pretty(&v)? + "\n")?;
    exclude_from_git(worktree, ".claude/settings.local.json")
}

/// Append `pattern` to the repo's shared `info/exclude` unless it is there.
fn exclude_from_git(worktree: &Path, pattern: &str) -> Result<()> {
    let common = git(worktree, &["rev-parse", "--git-common-dir"])?;
    let common = if Path::new(&common).is_absolute() { PathBuf::from(common) } else { worktree.join(common) };
    let exclude = common.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == pattern) {
        std::fs::create_dir_all(exclude.parent().expect("info dir"))?;
        let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&exclude, format!("{current}{sep}{pattern}\n"))?;
    }
    Ok(())
}

/// The main clone's untracked local files a session needs: the atlas, the
/// local instructions and the MCP config.
const LOCAL_FILES: [&str; 3] = ["embarch", "CLAUDE.local.md", ".mcp.json"];

/// Link each of `LOCAL_FILES` the main clone has into the worktree, so the
/// session loads the atlas and the MCP servers. A name the worktree already
/// has (a tracked file) is left alone. The excludes carry no trailing slash:
/// to git a symlink is not a directory.
pub fn link_local_files(main_clone: &Path, worktree: &Path) -> Result<()> {
    for name in LOCAL_FILES {
        let from = main_clone.join(name);
        let to = worktree.join(name);
        if from.symlink_metadata().is_err() || to.symlink_metadata().is_ok() {
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&from, &to)?;
        #[cfg(windows)]
        if from.is_dir() {
            std::os::windows::fs::symlink_dir(&from, &to)?;
        } else {
            std::os::windows::fs::symlink_file(&from, &to)?;
        }
        exclude_from_git(worktree, name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    /// A bare origin with `main` and a pushed issue branch, and a clone of it.
    fn fixture() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("desk-wt-{}", crate::paths::random_hex(4).unwrap()));
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        sh(&root, &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
        sh(&seed, &["init", "--quiet", "-b", "main"]);
        sh(&seed, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--quiet", "--allow-empty", "-m", "root"]);
        sh(&seed, &["remote", "add", "origin", origin.to_str().unwrap()]);
        sh(&seed, &["push", "--quiet", "origin", "main"]);
        sh(&seed, &["push", "--quiet", "origin", "main:user/abc-2-on-remote"]);
        let clone = root.join("proj").join("fw");
        std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
        sh(&root, &["clone", "--quiet", origin.to_str().unwrap(), clone.to_str().unwrap()]);
        (root, clone)
    }

    #[test]
    fn the_worktree_sits_beside_the_main_clone_named_after_the_issue() {
        assert_eq!(worktree_path(Path::new("/p/chip-fw"), "ABC-12"), PathBuf::from("/p/chip-fw-abc-12"));
    }

    #[test]
    fn a_new_branch_starts_from_origin_and_a_second_start_reuses_it() {
        let (root, clone) = fixture();
        let a = prepare(&clone, "ABC-1", "user/abc-1-new").unwrap();
        assert_eq!(a.path, worktree_path(&clone, "ABC-1"));
        assert!(a.how.starts_with("new from origin/"), "{}", a.how);
        let again = prepare(&clone, "ABC-1", "user/abc-1-new").unwrap();
        assert_eq!((again.how.as_str(), again.path), ("reused", a.path));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_branch_on_the_remote_is_checked_out_tracking_it() {
        let (root, clone) = fixture();
        let b = prepare(&clone, "ABC-2", "user/abc-2-on-remote").unwrap();
        assert_eq!(b.how, "remote branch");
        assert_eq!(git(&b.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), "user/abc-2-on-remote");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_remote_not_named_origin_is_the_one_used() {
        let (root, clone) = fixture();
        sh(&clone, &["remote", "rename", "origin", "acme"]);
        let a = prepare(&clone, "ABC-1", "user/abc-1-new").unwrap();
        assert!(a.how.starts_with("new from acme/"), "{}", a.how);
        let b = prepare(&clone, "ABC-2", "user/abc-2-on-remote").unwrap();
        assert_eq!(b.how, "remote branch");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_folder_in_the_way_is_never_touched() {
        let (root, clone) = fixture();
        std::fs::create_dir_all(worktree_path(&clone, "ABC-3")).unwrap();
        let err = prepare(&clone, "ABC-3", "user/abc-3").unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn auto_mode_is_local_and_excluded_from_git() {
        let (root, clone) = fixture();
        let w = prepare(&clone, "ABC-4", "user/abc-4").unwrap().path;
        set_auto_mode(&w).unwrap();
        set_auto_mode(&w).unwrap();
        let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(w.join(".claude/settings.local.json")).unwrap()).unwrap();
        assert_eq!(s["permissions"]["defaultMode"], "auto");
        assert_eq!(git(&w, &["status", "--porcelain"]).unwrap(), "", "the settings file is ignored");
        let exclude = std::fs::read_to_string(clone.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".claude/settings.local.json").count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_clones_local_files_are_linked_and_ignored() {
        let (root, clone) = fixture();
        std::fs::create_dir_all(clone.join("embarch")).unwrap();
        std::fs::write(clone.join("embarch/MAP.md"), "m").unwrap();
        std::fs::write(clone.join("CLAUDE.local.md"), "c").unwrap();
        let w = prepare(&clone, "ABC-5", "user/abc-5").unwrap().path;
        link_local_files(&clone, &w).unwrap();
        link_local_files(&clone, &w).unwrap();
        assert_eq!(std::fs::read_to_string(w.join("embarch/MAP.md")).unwrap(), "m");
        assert!(w.join("CLAUDE.local.md").symlink_metadata().unwrap().file_type().is_symlink());
        assert!(w.join(".mcp.json").symlink_metadata().is_err(), "the clone has no .mcp.json");
        assert_eq!(git(&w, &["status", "--porcelain"]).unwrap(), "", "the links are ignored");
        let exclude = std::fs::read_to_string(clone.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == "embarch").count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_clone_without_the_files_gets_no_links() {
        let (root, clone) = fixture();
        let w = prepare(&clone, "ABC-6", "user/abc-6").unwrap().path;
        link_local_files(&clone, &w).unwrap();
        for n in LOCAL_FILES {
            assert!(w.join(n).symlink_metadata().is_err(), "{n}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
