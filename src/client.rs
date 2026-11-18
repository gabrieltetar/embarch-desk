//! Calling a running desk: what the hook, the MCP server and the CLI share.

use crate::paths;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::time::Duration;

/// The Claude Code session a call comes from, and the folder it runs in
/// (EMB-94): the desk scopes the call to that session's own desk.
pub const SESSION_HEADER: &str = "x-embarch-session";
pub const CWD_HEADER: &str = "x-embarch-cwd";

pub struct DeskClient {
    http: reqwest::Client,
    base: String,
    token: String,
    caller: Option<(String, String)>,
}

/// The session this process runs in, from what Claude Code gives every MCP
/// server, hook and shell it starts, and the folder it runs in. `None`
/// outside a session: a terminal or a script works on the UI's desk.
fn caller() -> Option<(String, String)> {
    let sid = std::env::var("CLAUDE_CODE_SESSION_ID").ok().filter(|s| !s.trim().is_empty())?;
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    Some((sid.trim().to_string(), cwd))
}

impl DeskClient {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            base: paths::base_url(),
            token: paths::desk_token()?,
            caller: caller(),
        })
    }

    /// A client for one named instance: its own config folder (where its
    /// `desk-token` is) and port, each falling back to the environment and then
    /// the default instance, the way `serve` resolves them. The token is read,
    /// never generated: an instance that never ran has none to read.
    pub fn for_instance(timeout: Duration, config_dir: Option<&std::path::Path>, port: Option<u16>) -> Result<Self> {
        let token = match config_dir {
            None => paths::desk_token()?,
            Some(d) => {
                let f = d.join("desk-token");
                let t = std::fs::read_to_string(&f).map_err(|e| anyhow!("no desk token at {}: {e} (has that instance run?)", f.display()))?;
                t.trim().to_string()
            }
        };
        Ok(Self {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            base: port.map_or_else(paths::base_url, |p| format!("http://127.0.0.1:{p}")),
            token,
            caller: caller(),
        })
    }

    /// The bearer this client presents, so `get` can keep it out of its output.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
        if let Some((sid, cwd)) = &self.caller {
            req = req.header(SESSION_HEADER, sid.as_str());
            if let Ok(v) = reqwest::header::HeaderValue::from_bytes(cwd.as_bytes()) {
                req = req.header(CWD_HEADER, v);
            }
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() {
                anyhow!("the desk is not running at {} (start it with `embarch-desk service install`)", self.base)
            } else {
                anyhow!(e)
            }
        })?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}", v["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("HTTP {status}")));
        }
        Ok(v)
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::DELETE, path, None).await
    }
}
