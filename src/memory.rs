//! Claude Code's auto-memory files, as the work item's notes
//! (`embarch-chat-desk-proposal.md` 44, 45, 47).
//!
//! Sessions already keep a dated state file per issue in
//! `~/.claude/projects/<project>/memory/`, unprompted. The desk builds on
//! that habit rather than inventing one: **a write there becomes a note on
//! the session's work item** — the session decides the item, not the file's
//! name — and the files written before capture existed are attached once to
//! the items whose issue key they hold. `MEMORY.md`, the index, is never a
//! note.

use crate::store::{MemoryFile, MEMORY_TEXT_MAX};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Claude Code's own folder: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".claude"))
}

/// Is `path` a memory file — under `<claude_dir>/projects/*/memory/`, and
/// not the `MEMORY.md` index?
pub fn is_memory_file(path: &str, claude_dir: &Path) -> bool {
    let Ok(rest) = Path::new(path).strip_prefix(claude_dir.join("projects")) else { return false };
    let parts: Vec<&str> = rest.iter().filter_map(|c| c.to_str()).collect();
    parts.len() >= 3
        && parts[1] == "memory"
        && !parts.contains(&"..")
        && parts.last().is_some_and(|n| *n != "MEMORY.md")
}

/// A memory file's text, cut at [`MEMORY_TEXT_MAX`] characters; `None` when
/// it cannot be read. Only a regular file is read, and nothing below
/// `<claude_dir>/projects/` is followed through a symlink: a FIFO would
/// stall the desk on its open, and a link would pull a file from outside
/// the memory folders onto a card.
pub fn read_text(path: &Path, claude_dir: &Path) -> Option<String> {
    let base = claude_dir.join("projects");
    let mut at = base.clone();
    let mut meta = None;
    for c in path.strip_prefix(&base).ok()?.components() {
        at.push(c);
        let m = std::fs::symlink_metadata(&at).ok()?;
        if m.file_type().is_symlink() {
            return None;
        }
        meta = Some(m);
    }
    if !meta?.is_file() {
        return None;
    }
    // At most four bytes a character, so the cap is all that is read.
    let mut bytes = vec![];
    std::fs::File::open(path).ok()?.take(MEMORY_TEXT_MAX as u64 * 4).read_to_end(&mut bytes).ok()?;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        // Cut inside a character: what came before it.
        Err(e) if e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut b = e.into_bytes();
            b.truncate(valid);
            String::from_utf8(b).ok()?
        }
        Err(_) => return None,
    };
    Some(text.chars().take(MEMORY_TEXT_MAX).collect())
}

/// The folder Claude Code keeps a project's sessions and memory in, under
/// `projects/`: its path with every character but a letter or digit made `-`.
pub fn project_folder(path: &str) -> String {
    path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Is a project folder one of the ignored paths, or under one? A folder's
/// name cannot be read back — `/a/b-c` and `/a/b/c` name the same folder —
/// so a sibling sharing the prefix up to a dash (`/a/embarch-x` beside an
/// ignored `/a/embarch`) is skipped too: a starting state missed, never the
/// fleet's memory on a client's card.
fn project_ignored(folder: &str, ignore: &[String]) -> bool {
    ignore.iter().map(|i| project_folder(i.trim_end_matches('/'))).any(|i| {
        !i.is_empty() && (folder == i || folder.starts_with(&format!("{i}-")))
    })
}

/// Every memory file under `<claude_dir>/projects/*/memory/`, for the
/// one-time import — but those of projects under the ignore list, which is
/// untracked work (EmbArch's own repos, the fleet).
pub fn scan(claude_dir: &Path, ignore: &[String]) -> Vec<MemoryFile> {
    let mut out = vec![];
    let Ok(projects) = std::fs::read_dir(claude_dir.join("projects")) else { return out };
    for p in projects.flatten() {
        if project_ignored(&p.file_name().to_string_lossy(), ignore) {
            continue;
        }
        walk(&p.path().join("memory"), claude_dir, &mut out);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn walk(dir: &Path, claude_dir: &Path, out: &mut Vec<MemoryFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        // Not followed: a link is neither a folder nor a file here.
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            walk(&path, claude_dir, out);
            continue;
        }
        let p = path.display().to_string();
        if !meta.is_file() || !is_memory_file(&p, claude_dir) {
            continue;
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Some(text) = read_text(&path, claude_dir) {
            out.push(MemoryFile { path: p, mtime_ms, text });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_file_is_under_a_projects_memory_folder_and_not_the_index() {
        let c = Path::new("/home/u/.claude");
        assert!(is_memory_file("/home/u/.claude/projects/-w-fw/memory/abc-123-state.md", c));
        assert!(is_memory_file("/home/u/.claude/projects/-w-fw/memory/sub/notes.md", c));
        assert!(!is_memory_file("/home/u/.claude/projects/-w-fw/memory/MEMORY.md", c));
        assert!(!is_memory_file("/home/u/.claude/projects/-w-fw/abc.jsonl", c));
        assert!(!is_memory_file("/home/u/.claude/projects/-w-fw/memory", c));
        assert!(!is_memory_file("/w/fw/memory/abc.md", c));
        assert!(!is_memory_file("/home/u/.claude/projects/-w-fw/memory/../../x/memory/a.md", c));
    }

    #[test]
    fn the_scan_reads_every_memory_file_but_the_index() {
        let root = std::env::temp_dir().join(format!("desk-mem-{}", crate::paths::random_hex(4).unwrap()));
        let mem = root.join("projects").join("-w-fw").join("memory");
        std::fs::create_dir_all(mem.join("sub")).unwrap();
        std::fs::write(mem.join("MEMORY.md"), "- [ABC-1](abc-1.md)").unwrap();
        std::fs::write(mem.join("abc-1.md"), "ABC-1: branch user/abc-1, PR open").unwrap();
        std::fs::write(mem.join("sub").join("deep.md"), "nested").unwrap();
        std::fs::create_dir_all(root.join("projects").join("-w-other")).unwrap();
        let files = scan(&root, &[]);
        let names: Vec<&str> = files.iter().map(|f| f.path.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, ["abc-1.md", "deep.md"]);
        assert_eq!(files[0].text, "ABC-1: branch user/abc-1, PR open");
        assert!(files[0].mtime_ms > 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn scratch() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("desk-mem-{}", crate::paths::random_hex(4).unwrap()));
        let mem = root.join("projects").join("-w-fw").join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        (root, mem)
    }

    #[test]
    fn the_scan_never_reads_through_a_symlink() {
        let (root, mem) = scratch();
        let outside = root.join("connectors.toml");
        std::fs::write(&outside, "token = \"secret\"").unwrap();
        std::fs::write(mem.join("abc-1.md"), "ABC-1: branch").unwrap();
        std::os::unix::fs::symlink(&outside, mem.join("abc-2.md")).unwrap();
        // A memory folder that is itself a link reaches outside too.
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("abc-3.md"), "outside").unwrap();
        std::fs::create_dir_all(root.join("projects").join("-w-linked")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("projects").join("-w-linked").join("memory")).unwrap();
        let texts: Vec<String> = scan(&root, &[]).into_iter().map(|f| f.text).collect();
        assert_eq!(texts, ["ABC-1: branch"]);
        // The hook reads what a session wrote the same way.
        assert!(read_text(&mem.join("abc-2.md"), &root).is_none());
        assert_eq!(read_text(&mem.join("abc-1.md"), &root).as_deref(), Some("ABC-1: branch"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_long_file_is_read_only_up_to_the_cap_even_cut_inside_a_character() {
        let (root, mem) = scratch();
        std::fs::write(mem.join("long.md"), "€".repeat(MEMORY_TEXT_MAX * 2)).unwrap();
        let text = read_text(&mem.join("long.md"), &root).unwrap();
        assert_eq!(text, "€".repeat(MEMORY_TEXT_MAX));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_scan_skips_the_projects_of_ignored_paths() {
        let (root, mem) = scratch();
        std::fs::write(mem.join("abc-1.md"), "client work").unwrap();
        for project in ["-g-embarch", "-g-embarch-embarch-ui", "-g-embarch--chat-local"] {
            let m = root.join("projects").join(project).join("memory");
            std::fs::create_dir_all(&m).unwrap();
            std::fs::write(m.join("abc-1.md"), "fleet").unwrap();
        }
        assert_eq!(project_folder("/g/embarch/.chat-local"), "-g-embarch--chat-local");
        let texts: Vec<String> = scan(&root, &["/g/embarch/".to_string()]).into_iter().map(|f| f.text).collect();
        assert_eq!(texts, ["client work"]);
        assert!(!project_ignored("-g-embarchive", &["/g/embarch".to_string()]));
        assert!(!project_ignored("-w-fw", &["/".to_string()]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fifo_in_a_memory_folder_does_not_stall_the_scan() {
        let (root, mem) = scratch();
        std::fs::write(mem.join("abc-1.md"), "ABC-1: branch").unwrap();
        let fifo = mem.join("pipe.md");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let (tx, rx) = std::sync::mpsc::channel();
        let r = root.clone();
        std::thread::spawn(move || tx.send(scan(&r, &[]).len()).unwrap());
        let found = rx.recv_timeout(std::time::Duration::from_secs(2));
        // Unblock a scan stuck opening the FIFO before cleaning up.
        if found.is_err() {
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        }
        let read = found.is_ok().then(|| read_text(&fifo, &root));
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(found, Ok(1));
        assert_eq!(read, Some(None), "nor does the hook open one");
    }
}
