//! The pasted tokens, one per account, in `connectors.toml` (owner-only).
//!
//! v1 takes tokens, not OAuth (proposal decision 16): a GitHub token or the
//! user's existing `gh` login, a Linear personal API key per workspace, a
//! Harvest personal access token (which covers every account its user has),
//! and a Slack user token from an app the user made in their own workspace
//! from `slack-manifest.json`. **No token ever leaves this file except in the
//! request it authorizes** — the HTTP surface reports labels, never values.

use crate::paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const KINDS: [&str; 4] = ["linear", "harvest", "github", "slack"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub label: String,
    pub token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Connectors {
    #[serde(default)]
    pub linear: Vec<Account>,
    #[serde(default)]
    pub harvest: Vec<Account>,
    #[serde(default)]
    pub github: Vec<Account>,
    #[serde(default)]
    pub slack: Vec<Account>,
}

impl Connectors {
    fn path() -> Result<std::path::PathBuf> {
        Ok(paths::config_dir()?.join("connectors.toml"))
    }

    pub fn load() -> Result<Self> {
        let p = Self::path()?;
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let s = toml::to_string(self)?;
        paths::write_private(&Self::path()?, s.as_bytes())
    }

    pub fn list_mut(&mut self, kind: &str) -> Result<&mut Vec<Account>> {
        Ok(match kind {
            "linear" => &mut self.linear,
            "harvest" => &mut self.harvest,
            "github" => &mut self.github,
            "slack" => &mut self.slack,
            other => bail!("unknown connector kind {other:?}; one of {KINDS:?}"),
        })
    }

    /// Add or replace the account with this label.
    pub fn upsert(&mut self, kind: &str, label: &str, token: &str) -> Result<()> {
        let label = label.trim();
        let token = token.trim();
        if label.is_empty() || token.is_empty() {
            bail!("a connector needs a label and a token");
        }
        let list = self.list_mut(kind)?;
        list.retain(|a| a.label != label);
        list.push(Account { label: label.into(), token: token.into() });
        Ok(())
    }

    pub fn remove(&mut self, kind: &str, label: &str) -> Result<bool> {
        let list = self.list_mut(kind)?;
        let before = list.len();
        list.retain(|a| a.label != label);
        Ok(list.len() != before)
    }

    /// GitHub accounts, falling back to the user's `gh` login when none was
    /// pasted — the cheapest connector of the four.
    pub fn github_accounts(&self) -> Vec<Account> {
        if !self.github.is_empty() {
            return self.github.clone();
        }
        let out = std::process::Command::new("gh").args(["auth", "token"]).output();
        match out {
            Ok(o) if o.status.success() => {
                let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if t.is_empty() {
                    vec![]
                } else {
                    vec![Account { label: "gh login".into(), token: t }]
                }
            }
            _ => vec![],
        }
    }
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("embarch-desk (https://github.com/gabrieltetar/embarch-desk)")
        .build()
        .expect("reqwest client builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_by_label_and_rejects_blanks() {
        let mut c = Connectors::default();
        c.upsert("linear", "work", "a").unwrap();
        c.upsert("linear", "work", "b").unwrap();
        c.upsert("linear", "client", "c").unwrap();
        assert_eq!(c.linear.len(), 2);
        assert_eq!(c.linear[0].token, "b");
        assert!(c.upsert("slack", "", "x").is_err());
        assert!(c.upsert("jira", "a", "b").is_err());
        assert!(c.remove("linear", "work").unwrap());
        assert!(!c.remove("linear", "work").unwrap());
    }

    #[test]
    fn the_file_round_trips() {
        let mut c = Connectors::default();
        c.upsert("harvest", "me", "pat").unwrap();
        c.upsert("slack", "ws", "xoxp-1").unwrap();
        let back: Connectors = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }
}
