//! Whether Promptu is installed, for Start work's panel prompt (decision
//! 67): with it, Claude Code's panel gets `/promptu --item …`; without it,
//! the read-and-summarize starter it always had.
//!
//! Installed means the `promptu` binary is an executable file in the
//! suite's install folder (`~/.local/share/embarch/bin/`, which a user
//! service's PATH may lack) or on the desk's own `PATH` (the installing
//! shell's, carried into the unit, service.rs). Checked at each Start work,
//! never cached: a stat per folder. Claude Code's own plugin files are not
//! read, so a binary without the `embarch-promptu` plugin still counts — the
//! panel then gets a `/promptu` Claude Code does not know.
//!
//! The Desk tab's Promptu section (decision 65) runs the same binary: the
//! desk holds no Promptu logic, each route here runs one `promptu` command
//! and passes its JSON on. These are HTTP routes only; no MCP tool reaches
//! them, because an agent never marks or applies a sheet.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The folders searched, in order: the suite's install folder, then `PATH`.
pub fn search_dirs(home: Option<&Path>, path_env: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = home.map(|h| h.join(".local").join("share").join("embarch").join("bin")).into_iter().collect();
    if let Some(p) = path_env {
        // An empty entry is the current folder in a shell's lookup: a service's is `/`, never meant here.
        dirs.extend(std::env::split_paths(p).filter(|d| !d.as_os_str().is_empty()));
    }
    dirs
}

/// The first `promptu` that is an executable regular file (a symlink to one counts).
pub fn find(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join("promptu")).find(|p| executable(p))
}

/// Why a `promptu` command gave no answer.
#[derive(Debug, PartialEq)]
pub enum Fail {
    /// No binary: the message names it.
    Missing(String),
    /// The command refused (exit 2: an unknown week or line, a settled or superseded sheet); its own words.
    Refused(String),
    /// Any other failure: a crash, an exit past 2, output that is not JSON.
    Broken(String),
}

/// The folders searched on this machine, as the desk's process sees them.
pub fn current_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    search_dirs(home.as_deref(), std::env::var_os("PATH").as_deref())
}

/// An ISO week as Promptu names one, `YYYY-Www`.
pub fn valid_week(w: &str) -> bool {
    let b = w.as_bytes();
    b.len() == 8 && b[..4].iter().all(u8::is_ascii_digit) && &b[4..6] == b"-W" && b[6..].iter().all(u8::is_ascii_digit)
}

/// A line id (`l1`); a group id (`g1`) is one too, and nothing that could read as a flag.
pub fn valid_line(l: &str) -> bool {
    !l.is_empty() && l.len() <= 16 && l.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// Runs `promptu <args>` and parses its stdout as JSON. Exit 0 and 1 carry JSON (`week apply` exits 1 when a line failed, and says which in it).
pub async fn run(dirs: &[PathBuf], args: &[&str]) -> Result<Value, Fail> {
    let bin = find(dirs).ok_or_else(|| {
        Fail::Missing("promptu is not installed (no `promptu` in ~/.local/share/embarch/bin or on the desk's PATH)".into())
    })?;
    let out = tokio::process::Command::new(&bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| Fail::Broken(format!("running {}: {e}", bin.display())))?;
    let words = || String::from_utf8_lossy(&out.stderr).trim().to_string();
    match out.status.code() {
        Some(0 | 1) => serde_json::from_slice(&out.stdout).map_err(|e| Fail::Broken(format!("`promptu {}` printed no JSON: {e}", args.join(" ")))),
        Some(2) => Err(Fail::Refused(words())),
        c => Err(Fail::Broken(format!("`promptu {}` failed ({}): {}", args.join(" "), c.map_or("a signal".to_string(), |c| format!("exit {c}")), words()))),
    }
}

/// What `GET /promptu` serves: `promptu desk --json`, or nothing open when Promptu is absent or fails.
pub async fn desk(dirs: &[PathBuf]) -> Value {
    run(dirs, &["desk", "--json"]).await.unwrap_or_else(|_| json!({ "promptu": 1, "open": 0 }))
}

/// `GET /promptu/sheet`: the open sheet, or the named week's.
pub async fn sheet(dirs: &[PathBuf], week: Option<&str>) -> Result<Value, Fail> {
    match week {
        Some(w) => run(dirs, &["week", "show", "--week", w, "--json"]).await,
        None => run(dirs, &["week", "show", "--json"]).await,
    }
}

/// `POST /promptu/sheet/{week}/{line}`: `week mark`, approve or cut, with the owner's note.
pub async fn mark(dirs: &[PathBuf], week: &str, line: &str, verdict: &str, note: Option<&str>) -> Result<Value, Fail> {
    let mut a = vec!["week", "mark", line, "--verdict", verdict, "--week", week, "--json"];
    if let Some(n) = note.filter(|n| !n.is_empty()) {
        a.extend(["--note", n]);
    }
    run(dirs, &a).await
}

/// `POST /promptu/feel/{week}`: `week mark --feel`, which takes no line id.
pub async fn feel(dirs: &[PathBuf], week: &str, feel: &str) -> Result<Value, Fail> {
    run(dirs, &["week", "mark", "--feel", feel, "--week", week, "--json"]).await
}

/// `POST /promptu/apply/{week}`: `week apply`, each line's result in the answer.
pub async fn apply(dirs: &[PathBuf], week: &str) -> Result<Value, Fail> {
    run(dirs, &["week", "apply", "--week", week, "--json"]).await
}

#[cfg(unix)]
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

/// Promptu on this machine, as the desk's process sees it.
pub fn installed() -> bool {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    find(&search_dirs(home.as_deref(), std::env::var_os("PATH").as_deref())).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!("desk-promptu-{}", crate::paths::random_hex(4).unwrap()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn write(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn the_suite_folder_comes_first_then_path_and_empty_entries_are_dropped() {
        let dirs = search_dirs(Some(Path::new("/h")), Some(std::ffi::OsStr::new("/usr/bin::/h/.local/bin")));
        assert_eq!(dirs, [PathBuf::from("/h/.local/share/embarch/bin"), PathBuf::from("/usr/bin"), PathBuf::from("/h/.local/bin")]);
        assert_eq!(search_dirs(None, None), Vec::<PathBuf>::new());
    }

    #[cfg(unix)]
    #[test]
    fn only_an_executable_file_named_promptu_counts() {
        let d = scratch();
        let (suite, onpath, other) = (d.join("home/.local/share/embarch/bin"), d.join("bin"), d.join("other"));
        for x in [&suite, &onpath, &other] {
            std::fs::create_dir_all(x).unwrap();
        }
        let dirs = search_dirs(Some(&d.join("home")), Some(std::env::join_paths([&onpath, &other]).unwrap().as_os_str()));
        assert_eq!(find(&dirs), None, "absent everywhere");
        // A folder named promptu, and a file that is not executable, are not Promptu.
        std::fs::create_dir_all(suite.join("promptu")).unwrap();
        write(&onpath.join("promptu"), 0o644);
        assert_eq!(find(&dirs), None);
        write(&other.join("promptu"), 0o755);
        assert_eq!(find(&dirs), Some(other.join("promptu")), "on PATH");
        std::fs::remove_dir_all(suite.join("promptu")).unwrap();
        write(&suite.join("promptu"), 0o700);
        assert_eq!(find(&dirs), Some(suite.join("promptu")), "the suite's folder first");
        // A symlink to an executable counts; a dangling one does not.
        let link = d.join("link");
        std::fs::create_dir_all(&link).unwrap();
        std::os::unix::fs::symlink(other.join("promptu"), link.join("promptu")).unwrap();
        assert_eq!(find(std::slice::from_ref(&link)), Some(link.join("promptu")));
        std::fs::remove_file(other.join("promptu")).unwrap();
        assert_eq!(find(&[link]), None);
        std::fs::remove_dir_all(d).ok();
    }

    /// A fake `promptu` that logs its arguments and prints a canned answer by command.
    #[cfg(unix)]
    fn fake(d: &Path) -> Vec<PathBuf> {
        let bin = d.join("bin");
        write(&bin.join("promptu"), 0o755);
        let sh = format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
             'desk --json') echo '{{\"promptu\":1,\"open\":3,\"week\":\"2026-W48\"}}';;\n\
             'week show') echo '{{\"promptu\":1,\"week\":\"2026-W48\",\"groups\":[]}}';;\n\
             'week mark') case \"$3\" in --feel) echo '{{\"week\":\"2026-W48\",\"changed\":[],\"open\":2}}'; exit 0;; l9) echo 'no line l9' >&2; exit 2;; esac; echo '{{\"changed\":[{{\"id\":\"l1\",\"status\":\"cut\"}}],\"open\":2}}';;\n\
             'week apply') echo '{{\"applied\":1,\"failed\":1,\"lines\":[{{\"id\":\"l1\",\"status\":\"applied\"}},{{\"id\":\"l2\",\"status\":\"failed\"}}]}}'; exit 1;;\n\
             *) exit 3;;\nesac\n",
            log = d.join("calls.log").display()
        );
        std::fs::write(bin.join("promptu"), sh).unwrap();
        vec![bin]
    }

    fn calls(d: &Path) -> String {
        std::fs::read_to_string(d.join("calls.log")).unwrap_or_default()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn each_route_runs_its_promptu_command_and_passes_the_answer_on() {
        let d = scratch();
        let dirs = fake(&d);
        assert_eq!(desk(&dirs).await["open"], 3);
        assert_eq!(sheet(&dirs, None).await.unwrap()["week"], "2026-W48");
        assert_eq!(sheet(&dirs, Some("2026-W47")).await.unwrap()["promptu"], 1);
        let m = mark(&dirs, "2026-W48", "l1", "cut", Some("too broad")).await.unwrap();
        assert_eq!(m["changed"][0]["status"], "cut");
        mark(&dirs, "2026-W48", "l1", "approve", Some("")).await.unwrap();
        assert_eq!(feel(&dirs, "2026-W48", "better").await.unwrap()["open"], 2);
        // Exit 1 is a line that failed: the answer still carries every line's result.
        let a = apply(&dirs, "2026-W48").await.unwrap();
        assert_eq!(a["lines"][1]["status"], "failed");
        assert_eq!(
            calls(&d).lines().collect::<Vec<_>>(),
            [
                "desk --json",
                "week show --json",
                "week show --week 2026-W47 --json",
                "week mark l1 --verdict cut --week 2026-W48 --json --note too broad",
                "week mark l1 --verdict approve --week 2026-W48 --json",
                "week mark --feel better --week 2026-W48 --json",
                "week apply --week 2026-W48 --json",
            ]
        );
        // Exit 2 is a refusal in the command's words; anything else is a broken command.
        assert_eq!(mark(&dirs, "2026-W48", "l9", "cut", None).await, Err(Fail::Refused("no line l9".into())));
        std::fs::write(d.join("bin/promptu"), "#!/bin/sh\nexit 3\n").unwrap();
        assert!(matches!(apply(&dirs, "2026-W48").await, Err(Fail::Broken(_))));
        assert_eq!(desk(&dirs).await, json!({ "promptu": 1, "open": 0 }), "a failing command is nothing open");
        std::fs::remove_dir_all(d).ok();
    }

    #[tokio::test]
    async fn with_no_promptu_the_desk_answers_nothing_open_and_the_others_name_the_binary() {
        let d = scratch();
        let dirs = [d.join("empty")];
        assert_eq!(desk(&dirs).await, json!({ "promptu": 1, "open": 0 }));
        for r in [sheet(&dirs, None).await, mark(&dirs, "2026-W48", "l1", "cut", None).await, feel(&dirs, "2026-W48", "same").await, apply(&dirs, "2026-W48").await] {
            match r {
                Err(Fail::Missing(m)) => assert!(m.contains("promptu is not installed"), "{m}"),
                other => panic!("{other:?}"),
            }
        }
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn only_a_week_and_a_line_id_that_cannot_read_as_a_flag_are_passed_on() {
        assert!(valid_week("2026-W48") && !valid_week("2026-48") && !valid_week("--json") && !valid_week("2026-W4x"));
        assert!(valid_line("l1") && valid_line("g12") && !valid_line("") && !valid_line("--verdict") && !valid_line("l1 l2"));
    }
}
