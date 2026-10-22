//! Calling a running desk: what the hook, the MCP server and the CLI share.

use crate::paths;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::time::Duration;

pub struct DeskClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl DeskClient {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            base: paths::base_url(),
            token: paths::desk_token()?,
        })
    }

    pub async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
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
}
