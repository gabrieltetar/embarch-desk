//! Restacking a branch whose PR sits on another open PR, after that base
//! gained commits (EMB-142): rebase the branch onto the base's current tip,
//! list each conflicted hunk with file and line, resolve a hunk as ours,
//! theirs or neither without hand-editing markers, and continue only once the
//! diff against the base has been shown and no marker is left in a tracked
//! file. Local git only: **nothing here pushes, force or otherwise.**
//!
//! In a rebase "ours" is the side being built on, the base, and "theirs" is
//! the branch's own commit being replayed. No state is kept between calls;
//! everything is read back from the repo's `rebase-merge` folder.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Ours,
    Theirs,
    Neither,
}

impl Choice {
    pub fn parse(s: &str) -> Result<Choice> {
        match s.to_ascii_lowercase().as_str() {
            "ours" => Ok(Choice::Ours),
            "theirs" => Ok(Choice::Theirs),
            "neither" => Ok(Choice::Neither),
            other => bail!("choice must be ours, theirs or neither, not {other:?}"),
        }
    }
}

/// One conflict block: 1-based file lines of its first marker and its last.
#[derive(Debug, PartialEq, Eq)]
pub struct Hunk {
    pub start_line: usize,
    pub end_line: usize,
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
}

const OURS: &str = "<<<<<<< ";
const BASE: &str = "||||||| ";
const MID: &str = "=======";
const THEIRS: &str = ">>>>>>> ";

fn is_mid(l: &str) -> bool {
    l.trim_end_matches(['\r', '\n']) == MID
}

/// Every well-formed conflict block in `text`, top to bottom (a diff3 base
/// section is read and dropped).
pub fn parse_hunks(text: &str) -> Vec<Hunk> {
    #[derive(PartialEq)]
    enum S {
        Out,
        Ours,
        Base,
        Theirs,
    }
    let (mut out, mut s) = (Vec::new(), S::Out);
    let mut cur = Hunk { start_line: 0, end_line: 0, ours: vec![], theirs: vec![] };
    for (i, l) in text.split_inclusive('\n').enumerate() {
        let n = i + 1;
        match s {
            S::Out if l.starts_with(OURS) => {
                cur = Hunk { start_line: n, end_line: 0, ours: vec![], theirs: vec![] };
                s = S::Ours;
            }
            S::Ours | S::Base if is_mid(l) => s = S::Theirs,
            S::Ours if l.starts_with(BASE) => s = S::Base,
            S::Theirs if l.starts_with(THEIRS) => {
                cur.end_line = n;
                out.push(std::mem::replace(&mut cur, Hunk { start_line: 0, end_line: 0, ours: vec![], theirs: vec![] }));
                s = S::Out;
            }
            S::Ours => cur.ours.push(l.to_string()),
            S::Theirs => cur.theirs.push(l.to_string()),
            _ => {}
        }
    }
    out
}

/// `text` with hunk number `which` (1-based, as `parse_hunks` counts them) or
/// every hunk (`None`) replaced by the chosen side.
pub fn resolve_text(text: &str, which: Option<usize>, choice: Choice) -> Result<String> {
    let hunks = parse_hunks(text);
    if let Some(w) = which {
        if w == 0 || w > hunks.len() {
            bail!("hunk {w} does not exist: the file has {} conflicted hunk(s)", hunks.len());
        }
    } else if hunks.is_empty() {
        bail!("the file has no conflicted hunk");
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (i, h) in hunks.iter().enumerate() {
        if which.is_some_and(|w| w != i + 1) {
            continue;
        }
        for l in &lines[at..h.start_line - 1] {
            out.push_str(l);
        }
        match choice {
            Choice::Ours => h.ours.iter().for_each(|l| out.push_str(l)),
            Choice::Theirs => h.theirs.iter().for_each(|l| out.push_str(l)),
            Choice::Neither => {}
        }
        at = h.end_line;
    }
    for l in &lines[at..] {
        out.push_str(l);
    }
    Ok(out)
}

struct Run {
    ok: bool,
    out: String,
    err: String,
}

fn run(dir: &Path, args: &[&str]) -> Result<Run> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .output()
        .context("running git")?;
    Ok(Run { ok: o.status.success(), out: String::from_utf8_lossy(&o.stdout).into_owned(), err: String::from_utf8_lossy(&o.stderr).into_owned() })
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let r = run(dir, args)?;
    if !r.ok {
        bail!("git {}: {}", args.join(" "), r.err.trim());
    }
    Ok(r.out.trim_end().to_string())
}

fn toplevel(repo: Option<&str>) -> Result<PathBuf> {
    let dir = match repo {
        Some(r) => PathBuf::from(r),
        None => std::env::current_dir().context("no repo given and no working folder")?,
    };
    Ok(PathBuf::from(git(&dir, &["rev-parse", "--show-toplevel"])?))
}

fn git_path(root: &Path, p: &str) -> Result<PathBuf> {
    let s = git(root, &["rev-parse", "--git-path", p])?;
    let pb = PathBuf::from(&s);
    Ok(if pb.is_absolute() { pb } else { root.join(pb) })
}

fn rebase_in_progress(root: &Path) -> Result<bool> {
    Ok(git_path(root, "rebase-merge")?.is_dir() || git_path(root, "rebase-apply")?.is_dir())
}

fn unmerged_files(root: &Path) -> Result<Vec<String>> {
    Ok(git(root, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(str::to_string).collect())
}

/// Tracked files whose working copy still holds a conflict marker line.
fn files_with_markers(root: &Path) -> Result<Vec<String>> {
    let r = run(root, &["grep", "-l", "-I", "-E", "^(<<<<<<<|>>>>>>>) ", "--"])?;
    // exit 1 is "no match"
    Ok(r.out.lines().map(str::to_string).collect())
}

fn read(root: &Path, file: &str) -> Result<String> {
    let p = root.join(file);
    String::from_utf8(std::fs::read(&p).with_context(|| format!("reading {file}"))?).with_context(|| format!("{file} is not UTF-8 text"))
}

/// The conflicted hunks of every unmerged file, plus the unmerged files that
/// carry no marker (a delete/modify or binary conflict, left to the session).
fn conflicts(root: &Path) -> Result<Value> {
    let (mut hunks, mut markerless) = (Vec::new(), Vec::new());
    for f in unmerged_files(root)? {
        let hs = match read(root, &f) {
            Ok(t) => parse_hunks(&t),
            Err(_) => vec![],
        };
        if hs.is_empty() {
            markerless.push(f);
            continue;
        }
        for (i, h) in hs.iter().enumerate() {
            hunks.push(json!({ "file": f, "hunk": i + 1, "start_line": h.start_line, "end_line": h.end_line,
                               "ours": h.ours.concat(), "theirs": h.theirs.concat() }));
        }
    }
    Ok(json!({ "hunks": hunks, "unmerged_without_markers": markerless }))
}

fn state(root: &Path, extra: Value) -> Result<Value> {
    let mut v = if rebase_in_progress(root)? {
        let mut c = conflicts(root)?;
        c["status"] = json!("conflict");
        c["next"] = json!("desk_restack_resolve each hunk (the numbers are the file's current ones, so resolving hunk 1 renumbers the rest), \
            then desk_restack_continue to see the diff, then again with confirm=true; desk_restack_abort undoes it");
        c
    } else {
        json!({ "status": "rebased", "head": git(root, &["rev-parse", "HEAD"])?,
                "branch": git(root, &["rev-parse", "--abbrev-ref", "HEAD"])? })
    };
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    Ok(v)
}

/// Rebase `branch` onto `base`'s current tip (`--onto base old_base` when the
/// base was rewritten rather than extended).
pub fn start(repo: Option<&str>, branch: &str, base: &str, old_base: Option<&str>) -> Result<Value> {
    let root = toplevel(repo)?;
    if rebase_in_progress(&root)? {
        bail!("a rebase is already in progress here; continue it with desk_restack_continue or undo it with desk_restack_abort");
    }
    for r in [Some(branch), Some(base), old_base].into_iter().flatten() {
        git(&root, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")]).with_context(|| format!("no such ref: {r}"))?;
    }
    let onto = git(&root, &["rev-parse", base])?;
    let r = match old_base {
        Some(o) => run(&root, &["rebase", "--onto", base, o, branch])?,
        None => run(&root, &["rebase", base, branch])?,
    };
    if !r.ok && !rebase_in_progress(&root)? {
        bail!("git rebase: {}", format!("{}{}", r.out, r.err).trim());
    }
    state(&root, json!({ "onto": onto }))
}

/// Resolve hunk `hunk` of `file` (every hunk of it when `None`) as `choice`.
pub fn resolve(repo: Option<&str>, file: &str, hunk: Option<usize>, choice: Choice) -> Result<Value> {
    let root = toplevel(repo)?;
    if !rebase_in_progress(&root)? {
        bail!("no rebase is in progress here");
    }
    if !unmerged_files(&root)?.iter().any(|f| f == file) {
        bail!("{file} is not a conflicted file; the conflicted ones are {:?}", unmerged_files(&root)?);
    }
    let text = read(&root, file)?;
    let new = resolve_text(&text, hunk, choice)?;
    std::fs::write(root.join(file), new).with_context(|| format!("writing {file}"))?;
    let mut v = conflicts(&root)?;
    v["status"] = json!("conflict");
    v["resolved"] = json!({ "file": file, "hunk": hunk, "as": format!("{choice:?}").to_ascii_lowercase() });
    Ok(v)
}

/// Without `confirm`: check no marker is left and show the conflicted files'
/// diff against the base. With it: stage them and continue the rebase, which
/// may stop again on the next commit's conflict.
pub fn proceed(repo: Option<&str>, confirm: bool) -> Result<Value> {
    let root = toplevel(repo)?;
    if !rebase_in_progress(&root)? {
        bail!("no rebase is in progress here");
    }
    let left = files_with_markers(&root)?;
    if !left.is_empty() {
        bail!("conflict markers remain in {}; resolve every hunk first (desk_restack_resolve)", left.join(", "));
    }
    let files = unmerged_files(&root)?;
    let onto = std::fs::read_to_string(git_path(&root, "rebase-merge/onto")?).map(|s| s.trim().to_string()).context("reading the rebase's base")?;
    if !confirm {
        let mut args = vec!["diff", onto.as_str(), "--"];
        args.extend(files.iter().map(String::as_str));
        let diff = git(&root, &args)?;
        return Ok(json!({ "status": "ready", "files": files, "diff_against_base": diff,
                          "next": "check the diff, then desk_restack_continue again with confirm=true" }));
    }
    if !files.is_empty() {
        let mut args = vec!["add", "--"];
        args.extend(files.iter().map(String::as_str));
        git(&root, &args)?;
    }
    let r = run(&root, &["rebase", "--continue"])?;
    if !r.ok && !rebase_in_progress(&root)? {
        bail!("git rebase --continue: {}", format!("{}{}", r.out, r.err).trim());
    }
    state(&root, json!({ "onto": onto }))
}

/// Undo the restack: the branch goes back to where it was.
pub fn abort(repo: Option<&str>) -> Result<Value> {
    let root = toplevel(repo)?;
    if !rebase_in_progress(&root)? {
        bail!("no rebase is in progress here");
    }
    git(&root, &["rebase", "--abort"])?;
    Ok(json!({ "status": "aborted", "branch": git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])? }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("desk-restack-{}", crate::paths::random_hex(4).unwrap()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sh(d: &Path, args: &[&str]) {
        let o = Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    fn commit(d: &Path, file: &str, text: &str, msg: &str) {
        std::fs::write(d.join(file), text).unwrap();
        sh(d, &["add", file]);
        sh(d, &["commit", "--quiet", "-m", msg]);
    }

    /// `base` and `stack` both branch from `main`; `base` then gains a commit
    /// that rewrites lines 2 and 12 of f.txt, which `stack` also rewrites.
    fn fixture() -> PathBuf {
        let d = tmp();
        sh(&d, &["init", "--quiet", "-b", "main"]);
        commit(&d, "f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n", "root");
        sh(&d, &["checkout", "--quiet", "-b", "base"]);
        sh(&d, &["checkout", "--quiet", "-b", "stack"]);
        commit(&d, "f.txt", "1\nstack2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nstack12\n", "stack");
        sh(&d, &["checkout", "--quiet", "base"]);
        commit(&d, "f.txt", "1\nbase2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nbase12\n", "base fix");
        d
    }

    fn head_file(d: &Path) -> String {
        std::fs::read_to_string(d.join("f.txt")).unwrap()
    }

    #[test]
    fn a_clean_restack_moves_the_branch_onto_the_base_tip() {
        let d = tmp();
        sh(&d, &["init", "--quiet", "-b", "main"]);
        commit(&d, "a.txt", "a\n", "root");
        sh(&d, &["checkout", "--quiet", "-b", "base"]);
        sh(&d, &["checkout", "--quiet", "-b", "stack"]);
        commit(&d, "s.txt", "s\n", "stack work");
        sh(&d, &["checkout", "--quiet", "base"]);
        commit(&d, "b.txt", "b\n", "base gains a commit");
        let v = start(d.to_str(), "stack", "base", None).unwrap();
        assert_eq!(v["status"], "rebased", "{v}");
        assert_eq!(git(&d, &["rev-parse", "stack~1"]).unwrap(), git(&d, &["rev-parse", "base"]).unwrap());
        assert!(d.join("b.txt").exists() && d.join("s.txt").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_conflict_lists_every_hunk_with_file_and_line() {
        let d = fixture();
        let v = start(d.to_str(), "stack", "base", None).unwrap();
        assert_eq!(v["status"], "conflict", "{v}");
        let h = v["hunks"].as_array().unwrap();
        assert_eq!(h.len(), 2, "{v}");
        assert_eq!((h[0]["file"].as_str(), h[0]["hunk"].as_u64(), h[0]["start_line"].as_u64()), (Some("f.txt"), Some(1), Some(2)));
        assert_eq!(h[0]["ours"], "base2\n");
        assert_eq!(h[0]["theirs"], "stack2\n");
        assert_eq!(h[1]["hunk"], 2);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn each_hunk_resolves_as_ours_theirs_or_neither_then_the_diff_is_shown_before_continuing() {
        let d = fixture();
        start(d.to_str(), "stack", "base", None).unwrap();
        let v = resolve(d.to_str(), "f.txt", Some(1), Choice::Ours).unwrap();
        assert_eq!(v["hunks"].as_array().unwrap().len(), 1, "{v}");
        let v = resolve(d.to_str(), "f.txt", Some(1), Choice::Neither).unwrap();
        assert!(v["hunks"].as_array().unwrap().is_empty(), "{v}");
        assert_eq!(head_file(&d), "1\nbase2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n");
        let v = proceed(d.to_str(), false).unwrap();
        assert_eq!(v["status"], "ready", "{v}");
        let diff = v["diff_against_base"].as_str().unwrap();
        assert!(diff.contains("-base12"), "{diff}");
        assert!(proceed(d.to_str(), false).is_ok());
        let v = proceed(d.to_str(), true).unwrap();
        assert_eq!(v["status"], "rebased", "{v}");
        assert_eq!(head_file(&d), "1\nbase2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n");
        assert_eq!(git(&d, &["rev-parse", "stack~1"]).unwrap(), git(&d, &["rev-parse", "base"]).unwrap());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn theirs_keeps_the_branchs_side_and_a_whole_file_resolves_at_once() {
        let d = fixture();
        start(d.to_str(), "stack", "base", None).unwrap();
        let v = resolve(d.to_str(), "f.txt", None, Choice::Theirs).unwrap();
        assert!(v["hunks"].as_array().unwrap().is_empty(), "{v}");
        assert_eq!(head_file(&d), "1\nstack2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nstack12\n");
        assert_eq!(proceed(d.to_str(), true).unwrap()["status"], "rebased");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn it_refuses_to_continue_while_a_marker_remains_and_abort_restores_the_branch() {
        let d = fixture();
        let before = git(&d, &["rev-parse", "stack"]).unwrap();
        start(d.to_str(), "stack", "base", None).unwrap();
        resolve(d.to_str(), "f.txt", Some(1), Choice::Ours).unwrap();
        for confirm in [false, true] {
            let e = proceed(d.to_str(), confirm).unwrap_err().to_string();
            assert!(e.contains("markers remain in f.txt"), "{e}");
        }
        assert!(resolve(d.to_str(), "f.txt", Some(5), Choice::Ours).is_err());
        assert!(start(d.to_str(), "stack", "base", None).is_err(), "a second restack must not start");
        let v = abort(d.to_str()).unwrap();
        assert_eq!(v["status"], "aborted");
        assert_eq!(git(&d, &["rev-parse", "stack"]).unwrap(), before);
        assert!(proceed(d.to_str(), true).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_rewritten_base_restacks_with_old_base() {
        let d = tmp();
        sh(&d, &["init", "--quiet", "-b", "main"]);
        commit(&d, "a.txt", "a\n", "root");
        sh(&d, &["checkout", "--quiet", "-b", "base"]);
        commit(&d, "b.txt", "b1\n", "base work");
        sh(&d, &["checkout", "--quiet", "-b", "stack"]);
        commit(&d, "s.txt", "s\n", "stack work");
        sh(&d, &["branch", "old-base", "base"]);
        sh(&d, &["checkout", "--quiet", "base"]);
        sh(&d, &["commit", "--quiet", "--amend", "-m", "base work, reworded"]);
        let v = start(d.to_str(), "stack", "base", Some("old-base")).unwrap();
        assert_eq!(v["status"], "rebased", "{v}");
        assert_eq!(git(&d, &["rev-list", "--count", "base..stack"]).unwrap(), "1");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn hunk_text_handling_keeps_surrounding_lines_and_reads_diff3() {
        let t = "a\n<<<<<<< HEAD\no\n||||||| base\nb\n=======\nt\n>>>>>>> x\nz\n=======\n";
        let h = parse_hunks(t);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].start_line, h[0].end_line), (2, 8));
        assert_eq!(resolve_text(t, Some(1), Choice::Ours).unwrap(), "a\no\nz\n=======\n");
        assert_eq!(resolve_text(t, None, Choice::Theirs).unwrap(), "a\nt\nz\n=======\n");
        assert!(resolve_text("plain\n", None, Choice::Ours).is_err());
        assert!(Choice::parse("both").is_err());
    }
}
