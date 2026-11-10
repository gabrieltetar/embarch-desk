//! Whether a folder's Claude Code login has the `embarch-api` MCP server
//! (decision 82).
//!
//! Claude Code keeps MCP servers per login, in `<CLAUDE_CONFIG_DIR>/.claude.json`
//! (`~/.claude.json` with no `CLAUDE_CONFIG_DIR`): at user scope under
//! `mcpServers`, or per folder under `projects.<path>.mcpServers`; a folder's
//! own `.mcp.json` adds project servers. A login that registered the server
//! only per folder has none in a repo added later, nor in a worktree whose
//! path is new. **The desk only reads these files** — Claude Code rewrites
//! `.claude.json` while it runs, so a second writer could lose a write — and
//! says what command registers the server.

use serde_json::Value;
use std::path::{Path, PathBuf};

pub const SERVER: &str = "embarch-api";

/// One Claude Code login: the desk instance's `CLAUDE_CONFIG_DIR`.
#[derive(Debug, Clone)]
pub struct Login {
    /// `CLAUDE_CONFIG_DIR`, when the instance sets it.
    pub config_dir: Option<PathBuf>,
    /// The home folder, where the default login's `.claude.json` lives and
    /// the other logins' folders sit beside it.
    pub home: PathBuf,
}

impl Login {
    pub fn from_env() -> Option<Login> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)?;
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
        Some(Login { config_dir, home })
    }

    /// This login's `.claude.json`.
    pub fn file(&self) -> PathBuf {
        match &self.config_dir {
            Some(d) => d.join(".claude.json"),
            None => self.home.join(".claude.json"),
        }
    }

    /// How the login is named to a person.
    pub fn label(&self) -> String {
        match &self.config_dir {
            Some(d) => d.display().to_string(),
            None => self.home.join(".claude").display().to_string(),
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn names_server(servers: &Value) -> bool {
    servers.get(SERVER).is_some_and(Value::is_object)
}

/// Is `embarch-api` registered for `folder` in this login: user scope, the
/// folder's `projects` entry, or the folder's `.mcp.json`?
pub fn registered(login: &Login, folder: &str) -> bool {
    if let Some(cfg) = read_json(&login.file()) {
        if names_server(&cfg["mcpServers"]) || names_server(&cfg["projects"][folder]["mcpServers"]) {
            return true;
        }
    }
    read_json(&Path::new(folder).join(".mcp.json")).is_some_and(|m| names_server(&m["mcpServers"]))
}

/// An existing `embarch-api` entry's command and arguments, from this login's
/// `.claude.json`, the default login's, or a sibling `~/.claude*` login's:
/// user scope first, then any folder's.
fn existing_entry(login: &Login) -> Option<Vec<String>> {
    let mut files = vec![login.file(), login.home.join(".claude.json")];
    if let Ok(rd) = std::fs::read_dir(&login.home) {
        let mut sib: Vec<PathBuf> =
            rd.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with(".claude")).map(|e| e.path().join(".claude.json")).collect();
        sib.sort();
        files.extend(sib);
    }
    let entry_words = |e: &Value| -> Option<Vec<String>> {
        let mut w = vec![e.get("command")?.as_str()?.to_string()];
        w.extend(e.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.as_str().map(String::from)));
        Some(w)
    };
    let cfgs: Vec<Value> = files.iter().filter_map(|f| read_json(f)).collect();
    cfgs.iter()
        .find_map(|c| entry_words(&c["mcpServers"][SERVER]))
        .or_else(|| cfgs.iter().find_map(|c| c["projects"].as_object()?.values().find_map(|p| entry_words(&p["mcpServers"][SERVER]))))
}

fn shell_word(w: &str) -> String {
    let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// The command that registers the server at user scope, which covers every
/// worktree. The binary and config come from an existing entry when there is
/// one; otherwise the two values are left for the user to fill in.
pub fn register_command(login: &Login) -> String {
    let prefix = login.config_dir.as_ref().map(|d| format!("CLAUDE_CONFIG_DIR={} ", shell_word(&d.display().to_string()))).unwrap_or_default();
    let tail = match existing_entry(login) {
        Some(words) => words.iter().map(|w| shell_word(w)).collect::<Vec<_>>().join(" "),
        None => "<path to the embarch-api binary> --config <path to its config file>".to_string(),
    };
    format!("{prefix}claude mcp add --scope user {SERVER} -- {tail}")
}

/// The line a session starting in `folder` is told when its login lacks the
/// server, or `None` when it has it.
pub fn session_text(login: &Login, folder: &str) -> Option<String> {
    if registered(login, folder) {
        return None;
    }
    Some(format!(
        "EmbArch Desk: the Claude Code login at {} has no `{SERVER}` MCP server for {folder}, so this session has no \
         build, flash or bootload tools. Tell the user. The command that adds it for every folder of that login, to \
         run in a terminal and then restart the session: {}",
        login.label(),
        register_command(login)
    ))
}

/// A desk folder's entry in the desk's answer.
pub fn folder_json(login: &Login, folder: &str) -> Value {
    if registered(login, folder) {
        serde_json::json!({ "folder": folder, "registered": true })
    } else {
        serde_json::json!({ "folder": folder, "registered": false, "login": login.label(), "register_command": register_command(login) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Scratch {
            let d = std::env::temp_dir().join(format!("desk-mcpreg-{}", crate::paths::random_hex(4).unwrap()));
            std::fs::create_dir_all(d.join("home/.claude-work")).unwrap();
            std::fs::create_dir_all(d.join("repo")).unwrap();
            Scratch(d)
        }
        fn login(&self) -> Login {
            Login { config_dir: Some(self.0.join("home/.claude-work")), home: self.0.join("home") }
        }
        fn repo(&self) -> String {
            self.0.join("repo").display().to_string()
        }
        fn put(&self, rel: &str, v: Value) {
            std::fs::write(self.0.join(rel), v.to_string()).unwrap();
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn server() -> Value {
        serde_json::json!({ "embarch-api": { "command": "/b/embarch-api", "args": ["--config", "/c/api.toml"] } })
    }

    #[test]
    fn missing_everywhere_is_not_registered() {
        let s = Scratch::new();
        assert!(!registered(&s.login(), &s.repo()), "no files at all");
        s.put("home/.claude-work/.claude.json", serde_json::json!({ "mcpServers": { "other": {} }, "projects": { s.repo(): { "mcpServers": {} } } }));
        assert!(!registered(&s.login(), &s.repo()));
        s.put("repo/.mcp.json", serde_json::json!({ "mcpServers": { "other": {} } }));
        assert!(!registered(&s.login(), &s.repo()));
    }

    #[test]
    fn user_scope_counts() {
        let s = Scratch::new();
        s.put("home/.claude-work/.claude.json", serde_json::json!({ "mcpServers": server() }));
        assert!(registered(&s.login(), &s.repo()));
    }

    #[test]
    fn the_folders_own_projects_entry_counts_and_only_for_that_folder() {
        let s = Scratch::new();
        s.put("home/.claude-work/.claude.json", serde_json::json!({ "projects": { s.repo(): { "mcpServers": server() } } }));
        assert!(registered(&s.login(), &s.repo()));
        assert!(!registered(&s.login(), &format!("{}-worktree", s.repo())), "a new worktree path has no entry");
    }

    #[test]
    fn the_folders_mcp_json_counts() {
        let s = Scratch::new();
        s.put("repo/.mcp.json", serde_json::json!({ "mcpServers": server() }));
        assert!(registered(&s.login(), &s.repo()));
    }

    #[test]
    fn another_logins_file_does_not_count() {
        let s = Scratch::new();
        s.put("home/.claude.json", serde_json::json!({ "mcpServers": server() }));
        assert!(!registered(&s.login(), &s.repo()), "the instance's own login decides");
    }

    #[test]
    fn the_command_takes_binary_and_config_from_either_login_else_asks() {
        let s = Scratch::new();
        let blank = register_command(&s.login());
        assert!(blank.contains("<path to the embarch-api binary> --config <path to its config file>"), "{blank}");
        assert!(blank.starts_with(&format!("CLAUDE_CONFIG_DIR={}/home/.claude-work claude mcp add --scope user embarch-api -- ", s.0.display())), "{blank}");
        s.put("home/.claude.json", serde_json::json!({ "projects": { "/x": { "mcpServers": server() } } }));
        assert!(register_command(&s.login()).ends_with("embarch-api -- /b/embarch-api --config /c/api.toml"));
    }

    #[test]
    fn a_session_is_told_once_per_start_only_where_the_server_is_missing() {
        let s = Scratch::new();
        let told = session_text(&s.login(), &s.repo()).unwrap();
        assert!(told.contains(&s.login().label()) && told.contains("claude mcp add --scope user embarch-api"), "{told}");
        s.put("repo/.mcp.json", serde_json::json!({ "mcpServers": server() }));
        assert!(session_text(&s.login(), &s.repo()).is_none());
    }

    #[test]
    fn nothing_is_ever_written() {
        let s = Scratch::new();
        s.put("home/.claude-work/.claude.json", serde_json::json!({ "projects": {} }));
        let before = std::fs::read(s.0.join("home/.claude-work/.claude.json")).unwrap();
        let _ = (registered(&s.login(), &s.repo()), register_command(&s.login()), session_text(&s.login(), &s.repo()));
        assert_eq!(std::fs::read(s.0.join("home/.claude-work/.claude.json")).unwrap(), before);
        assert!(!s.0.join("repo/.mcp.json").exists());
    }

    #[test]
    fn words_with_spaces_are_quoted() {
        assert_eq!(shell_word("a b"), "'a b'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word("/p/x.toml"), "/p/x.toml");
    }
}
