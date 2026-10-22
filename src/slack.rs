//! Slack with a user token: every @mention and DM in every connected
//! workspace, unread first (proposal decision 14), and a reply sent as the
//! user once its draft is approved.
//!
//! Slack has no personal access token. The token comes from an app the user
//! makes in their own workspace from `slack-manifest.json` — or, in a
//! workspace that already has the EmbArch fleet's app, from that app given
//! the same user scopes.

use crate::connectors::{http, Account};
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

const API: &str = "https://slack.com/api";
/// How far back the feed looks.
const WINDOW_S: i64 = 3 * 86_400;

#[derive(Debug, Clone, Serialize)]
pub struct FeedItem {
    pub account: String,
    pub kind: String,
    pub channel: String,
    pub channel_name: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user_name: String,
    pub text: String,
    pub permalink: String,
    pub unread: bool,
}

async fn call(acct: &Account, method: &str, params: &[(&str, String)]) -> Result<Value> {
    let resp = http()
        .get(format!("{API}/{method}"))
        .bearer_auth(&acct.token)
        .query(params)
        .send()
        .await?;
    let v: Value = resp.json().await?;
    if v["ok"].as_bool() != Some(true) {
        bail!("slack {method} ({}): {}", acct.label, v["error"].as_str().unwrap_or("unknown error"));
    }
    Ok(v)
}

pub async fn whoami(acct: &Account) -> Result<(String, String, String)> {
    let v = call(acct, "auth.test", &[]).await?;
    Ok((
        v["user_id"].as_str().unwrap_or_default().to_string(),
        v["team"].as_str().unwrap_or("?").to_string(),
        v["url"].as_str().unwrap_or_default().to_string(),
    ))
}

fn permalink(team_url: &str, channel: &str, ts: &str, thread_ts: Option<&str>) -> String {
    let base = format!("{}archives/{channel}/p{}", team_url, ts.replace('.', ""));
    match thread_ts {
        Some(t) if t != ts => format!("{base}?thread_ts={t}&cid={channel}"),
        _ => base,
    }
}

fn ts_secs(ts: &str) -> f64 {
    ts.parse().unwrap_or(0.0)
}

/// One workspace's mentions and DMs, newest first within unread-first.
pub async fn feed(acct: &Account) -> Result<Vec<FeedItem>> {
    let (me, _team, team_url) = whoami(acct).await?;
    let now = chrono::Utc::now().timestamp();
    let oldest = (now - WINDOW_S).to_string();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut last_read: HashMap<String, f64> = HashMap::new();
    let mut out = vec![];

    let found = call(
        acct,
        "search.messages",
        &[("query", format!("<@{me}>")), ("sort", "timestamp".into()), ("count", "20".into())],
    )
    .await?;
    for m in found["messages"]["matches"].as_array().into_iter().flatten() {
        if ts_secs(m["ts"].as_str().unwrap_or("0")) < (now - WINDOW_S) as f64 || m["user"].as_str() == Some(me.as_str()) {
            continue;
        }
        let channel = m["channel"]["id"].as_str().unwrap_or_default().to_string();
        out.push(FeedItem {
            account: acct.label.clone(),
            kind: "mention".into(),
            channel_name: m["channel"]["name"].as_str().unwrap_or(&channel).to_string(),
            channel,
            ts: m["ts"].as_str().unwrap_or_default().to_string(),
            thread_ts: m["permalink"]
                .as_str()
                .and_then(|p| p.split("thread_ts=").nth(1))
                .map(|t| t.split('&').next().unwrap_or(t).to_string()),
            user_name: m["username"].as_str().unwrap_or("?").to_string(),
            text: m["text"].as_str().unwrap_or_default().to_string(),
            permalink: m["permalink"].as_str().unwrap_or_default().to_string(),
            unread: false,
        });
    }

    let ims = call(acct, "conversations.list", &[("types", "im,mpim".into()), ("limit", "100".into())]).await?;
    let mut chans: Vec<&Value> = ims["channels"].as_array().into_iter().flatten().collect();
    chans.sort_by_key(|c| std::cmp::Reverse(c["updated"].as_i64().unwrap_or(0)));
    for c in chans.into_iter().take(20) {
        let id = c["id"].as_str().unwrap_or_default().to_string();
        let hist = match call(acct, "conversations.history", &[("channel", id.clone()), ("oldest", oldest.clone()), ("limit", "5".into())]).await {
            Ok(h) => h,
            Err(_) => continue,
        };
        let peer = c["user"].as_str().unwrap_or_default().to_string();
        for m in hist["messages"].as_array().into_iter().flatten() {
            let from = m["user"].as_str().unwrap_or_default();
            if from == me || from.is_empty() {
                continue;
            }
            if !names.contains_key(from) {
                let n = call(acct, "users.info", &[("user", from.to_string())])
                    .await
                    .ok()
                    .and_then(|u| {
                        u["user"]["profile"]["display_name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or(u["user"]["real_name"].as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| from.to_string());
                names.insert(from.to_string(), n);
            }
            let ts = m["ts"].as_str().unwrap_or_default().to_string();
            let thread = m["thread_ts"].as_str().map(str::to_string);
            out.push(FeedItem {
                account: acct.label.clone(),
                kind: "dm".into(),
                channel: id.clone(),
                channel_name: if peer.is_empty() { c["name"].as_str().unwrap_or("group").to_string() } else { "DM".into() },
                permalink: permalink(&team_url, &id, &ts, thread.as_deref()),
                thread_ts: thread,
                ts,
                user_name: names[from].clone(),
                text: m["text"].as_str().unwrap_or_default().to_string(),
                unread: false,
            });
        }
    }

    // Unread: a message newer than its conversation's read marker. One
    // conversations.info per distinct channel, capped.
    let mut chans: Vec<String> = out.iter().map(|f| f.channel.clone()).collect();
    chans.sort();
    chans.dedup();
    for ch in chans.into_iter().take(25) {
        if let Ok(info) = call(acct, "conversations.info", &[("channel", ch.clone())]).await {
            if let Some(lr) = info["channel"]["last_read"].as_str() {
                last_read.insert(ch, ts_secs(lr));
            }
        }
    }
    for f in &mut out {
        f.unread = last_read.get(&f.channel).is_some_and(|lr| ts_secs(&f.ts) > *lr);
    }
    sort_feed(&mut out);
    Ok(out)
}

pub fn sort_feed(items: &mut [FeedItem]) {
    items.sort_by(|a, b| b.unread.cmp(&a.unread).then(ts_secs(&b.ts).total_cmp(&ts_secs(&a.ts))));
}

/// Send an approved reply as the user, in the thread it answers.
pub async fn reply(acct: &Account, channel: &str, thread_ts: Option<&str>, text: &str) -> Result<String> {
    let resp = http()
        .post(format!("{API}/chat.postMessage"))
        .bearer_auth(&acct.token)
        .json(&serde_json::json!({ "channel": channel, "text": text, "thread_ts": thread_ts }))
        .send()
        .await?;
    let v: Value = resp.json().await?;
    if v["ok"].as_bool() != Some(true) {
        bail!("slack refused the reply: {}", v["error"].as_str().unwrap_or("?"));
    }
    Ok(v["ts"].as_str().unwrap_or_default().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ts: &str, unread: bool) -> FeedItem {
        FeedItem {
            account: "a".into(),
            kind: "dm".into(),
            channel: "D1".into(),
            channel_name: "DM".into(),
            ts: ts.into(),
            thread_ts: None,
            user_name: "x".into(),
            text: "hi".into(),
            permalink: String::new(),
            unread,
        }
    }

    #[test]
    fn unread_comes_first_then_newest() {
        let mut v = vec![item("100.1", false), item("50.0", true), item("200.0", false), item("60.0", true)];
        sort_feed(&mut v);
        let order: Vec<_> = v.iter().map(|f| f.ts.as_str()).collect();
        assert_eq!(order, ["60.0", "50.0", "200.0", "100.1"]);
    }

    #[test]
    fn permalinks_point_into_the_thread() {
        assert_eq!(permalink("https://t.slack.com/", "D1", "1.5", None), "https://t.slack.com/archives/D1/p15");
        assert_eq!(
            permalink("https://t.slack.com/", "D1", "2.5", Some("1.5")),
            "https://t.slack.com/archives/D1/p25?thread_ts=1.5&cid=D1"
        );
    }
}
