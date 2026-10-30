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
}
