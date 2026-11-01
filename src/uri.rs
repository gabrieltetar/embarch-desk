//! Percent-encoding and the two `vscode://` links a card's launch button
//! follows: one opening the work item's repo, one opening the Claude Code
//! panel with its prompt pre-filled (documented as pre-filled, never
//! auto-submitted).

pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Under WSL the folder has to be opened through VS Code's remote, or the
/// Windows side looks for a Linux path on C:. `windowId=_blank` opens it in a
/// new window: without it VS Code swaps the folder into the last-focused
/// window, and the chat running there ends (decision 25). A folder already
/// open keeps its window, which is brought up instead.
pub fn folder_uri(path: &str, wsl_distro: Option<&str>) -> String {
    let path_enc: String = path.split('/').map(encode).collect::<Vec<_>>().join("/");
    match wsl_distro {
        Some(d) => format!("vscode://vscode-remote/wsl+{}{}?windowId=_blank", encode(d), path_enc),
        None => format!("vscode://file{path_enc}?windowId=_blank"),
    }
}

pub fn claude_uri(prompt: &str) -> String {
    format!("vscode://anthropic.claude-code/open?prompt={}", encode(prompt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_paths_go_through_the_remote() {
        assert_eq!(folder_uri("/home/g/My Repo", Some("Ubuntu")), "vscode://vscode-remote/wsl+Ubuntu/home/g/My%20Repo?windowId=_blank");
        assert_eq!(folder_uri("/home/g/r", None), "vscode://file/home/g/r?windowId=_blank");
        assert_eq!(claude_uri("Work on ABC-1: x&y"), "vscode://anthropic.claude-code/open?prompt=Work%20on%20ABC-1%3A%20x%26y");
    }
}
