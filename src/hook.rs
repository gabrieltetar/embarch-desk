//! `embarch-desk hook`: the one command every hook in the plugin runs.
//!
//! It forwards the hook's stdin to the desk and, for the two events whose
//! output Claude reads, prints the desk's context back. **It never fails a
//! session**: no desk, a slow desk, a malformed event — all exit 0 with
//! nothing printed, and the time for those events is simply not counted.

use crate::client::DeskClient;
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

/// What Claude Code reads from a SessionStart or UserPromptSubmit hook.
pub fn output_for(event: &str, context: Option<&str>) -> Option<String> {
    let ctx = context.filter(|c| !c.is_empty())?;
    matches!(event, "SessionStart" | "UserPromptSubmit").then(|| {
        json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": ctx } }).to_string()
    })
}

pub fn run() {
    // The desk's own `claude -p` calls must not become sessions.
    if std::env::var_os("EMBARCH_DESK_INTERNAL").is_some() {
        return;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(ev) = serde_json::from_str::<Value>(&input) else { return };
    let event = ev["hook_event_name"].as_str().unwrap_or_default().to_string();
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    let reply = rt.block_on(async {
        let client = DeskClient::new(Duration::from_millis(1500)).ok()?;
        client.post("/hook", ev).await.ok()
    });
    if let Some(out) = reply.and_then(|r| output_for(&event, r["context"].as_str())) {
        println!("{out}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_reading_events_print_and_only_with_something_to_say() {
        let s = output_for("UserPromptSubmit", Some("ask")).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "ask");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert!(output_for("PostToolUse", Some("x")).is_none());
        assert!(output_for("SessionStart", None).is_none());
        assert!(output_for("SessionStart", Some("")).is_none());
    }
}
