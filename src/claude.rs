//! Claude, called as `claude -p` through the user's own install and login
//! (proposal decision 19).
//!
//! **Stripped to the prompt, or it costs 300 times as much.** Measured
//! 2026-10-22: a bare `claude -p` loaded every plugin, MCP server and memory
//! file the user has — 99,290 tokens of cache creation, $0.20 to say "OK".
//! With no tools, no MCP, no settings sources and a replaced system prompt
//! the same call was 493 input tokens and $0.0007. `--bare` would be the
//! obvious flag and does not work: it skips the login too, and the call
//! ends in `api_error`.
//!
//! `EMBARCH_DESK_INTERNAL=1` is set on the child so that, whatever settings
//! do load, the desk's own hooks drop the session instead of tracking it.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

pub const DEFAULT_MODEL: &str = "sonnet";

pub async fn ask(system: &str, prompt: &str, model: &str) -> Result<String> {
    let cwd = crate::paths::data_dir()?;
    std::fs::create_dir_all(&cwd)?;
    let mut child = tokio::process::Command::new("claude")
        .current_dir(&cwd)
        .env("EMBARCH_DESK_INTERNAL", "1")
        .args(["-p", "--model", model, "--output-format", "json", "--strict-mcp-config"])
        .args(["--tools", ""])
        .args(["--setting-sources", ""])
        .args(["--system-prompt", system])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("could not run `claude` — is Claude Code installed and on PATH?")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(prompt.as_bytes())
        .await?;
    let out = tokio::time::timeout(std::time::Duration::from_secs(180), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("claude -p took longer than 3 minutes"))??;
    let v: Value = serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "claude -p returned no JSON (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).chars().take(400).collect::<String>()
        )
    })?;
    if v["is_error"].as_bool() == Some(true) || v["terminal_reason"].as_str().is_some_and(|r| r != "completed") {
        bail!("claude -p failed: {}", v["result"].as_str().or(v["terminal_reason"].as_str()).unwrap_or("?"));
    }
    v["result"].as_str().map(str::to_string).ok_or_else(|| anyhow!("claude -p returned no result"))
}

/// The first JSON object in a reply, fenced or not.
pub fn first_json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in text[start..].char_indices() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..start + i + 1]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_found_inside_a_fenced_reply() {
        let r = "Here you go:\n```json\n{\"note\": \"Fixed {the} race\", \"project_id\": 3}\n```";
        let v = first_json_object(r).unwrap();
        assert_eq!(v["note"], "Fixed {the} race");
        assert_eq!(v["project_id"], 3);
        assert!(first_json_object("no json").is_none());
    }
}
