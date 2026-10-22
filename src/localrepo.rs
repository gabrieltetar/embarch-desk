//! Finding a work item's clone on disk from its PR's GitHub repo — the
//! second of the three ways a card learns where to open (proposal decision
//! 8). Reads `.git/config` directly rather than spawning git per directory.

use std::path::{Path, PathBuf};

const SKIP: &[&str] = &["node_modules", "target", "build", "zephyr", "modules", "bootloader", "tools"];

fn origin_matches(git_config: &str, full_name: &str) -> bool {
    let want = full_name.trim_end_matches(".git").to_ascii_lowercase();
    git_config.lines().any(|l| {
        let l = l.trim();
        l.starts_with("url") && {
            let url = l.split_once('=').map(|(_, u)| u.trim()).unwrap_or("");
            let url = url.trim_end_matches('/').trim_end_matches(".git").to_ascii_lowercase();
            url.ends_with(&format!("/{want}")) || url.ends_with(&format!(":{want}"))
        }
    })
}

fn scan(dir: &Path, full_name: &str, depth: usize, out: &mut Option<PathBuf>) {
    if out.is_some() {
        return;
    }
    if let Ok(cfg) = std::fs::read_to_string(dir.join(".git").join("config")) {
        if origin_matches(&cfg, full_name) {
            *out = Some(dir.to_path_buf());
        }
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            scan(&e.path(), full_name, depth - 1, out);
        }
    }
}

/// The first clone under the usual roots whose origin is `owner/repo`.
pub fn find_clone(full_name: &str) -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let mut out = None;
    for root in ["Github", "github", "src", "code", "repos", "projects"] {
        let r = home.join(root);
        if r.is_dir() {
            scan(&r, full_name, 3, &mut out);
        }
        if out.is_some() {
            break;
        }
    }
    out.map(|p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_match_in_ssh_and_https_forms() {
        let ssh = "[remote \"origin\"]\n\turl = git@github.com-personal:gabrieltetar/embarch-doc.git\n";
        let https = "[remote \"origin\"]\n\turl = https://github.com/example-org/fw.git\n";
        assert!(origin_matches(ssh, "gabrieltetar/embarch-doc"));
        assert!(origin_matches(https, "example-org/FW"));
        assert!(!origin_matches(https, "example-org/fw-tools"));
    }

    #[test]
    fn a_clone_is_found_by_its_origin() {
        let root = std::env::temp_dir().join(format!("desk-repo-{}", crate::paths::random_hex(4).unwrap()));
        let repo = root.join("a").join("b");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/config"), "url = git@github.com:o/r.git\n").unwrap();
        let mut out = None;
        scan(&root, "o/r", 3, &mut out);
        assert_eq!(out, Some(repo));
        std::fs::remove_dir_all(root).unwrap();
    }
}
