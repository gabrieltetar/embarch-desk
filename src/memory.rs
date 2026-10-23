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
/// it cannot be read.
pub fn read_text(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.chars().take(MEMORY_TEXT_MAX).collect())
}

/// Every memory file under `<claude_dir>/projects/*/memory/`, for the
/// one-time import.
pub fn scan(claude_dir: &Path) -> Vec<MemoryFile> {
    let mut out = vec![];
    let Ok(projects) = std::fs::read_dir(claude_dir.join("projects")) else { return out };
    for p in projects.flatten() {
        walk(&p.path().join("memory"), claude_dir, &mut out);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn walk(dir: &Path, claude_dir: &Path, out: &mut Vec<MemoryFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            walk(&path, claude_dir, out);
            continue;
        }
        let p = path.display().to_string();
        if !is_memory_file(&p, claude_dir) {
            continue;
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Some(text) = read_text(&path) {
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
        let files = scan(&root);
        let names: Vec<&str> = files.iter().map(|f| f.path.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, ["abc-1.md", "deep.md"]);
        assert_eq!(files[0].text, "ABC-1: branch user/abc-1, PR open");
        assert!(files[0].mtime_ms > 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
