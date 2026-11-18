//! `embarch-desk get <route>`: what a route returns on a running desk, read
//! from a shell without handling the token. By default the answer's shape
//! (keys, list lengths, one sample row, optionally a count by a field); `--raw`
//! the JSON itself. Either way no secret is printed: a value under a key that
//! names one is masked, and so is the desk's own token wherever it appears.

use crate::client::DeskClient;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const MASK: &str = "***";

/// A key whose value is a secret, by name. Matched on the lowercase key, so
/// `refresh_token`, `Authorization` and `api_key` all count.
fn secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["token", "secret", "password", "authorization", "api_key", "apikey", "bearer", "credential"].iter().any(|w| k.contains(w))
}

/// Mask in place every value under a secret-named key (whatever its type, a
/// whole object included), and the desk's own token inside any string.
pub fn mask(v: &mut Value, bearer: &str) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if secret_key(k) && !x.is_null() {
                    *x = json!(MASK);
                } else {
                    mask(x, bearer);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| mask(x, bearer)),
        Value::String(s) if !bearer.is_empty() && s.contains(bearer) => *s = s.replace(bearer, MASK),
        _ => {}
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Array(_) => "list",
        Value::Object(_) => "object",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "bool",
        Value::Null => "null",
    }
}

fn counts(rows: &[Value], field: &str) -> String {
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for r in rows {
        let k = match r.get(field) {
            None => "(absent)".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(o) => o.to_string(),
        };
        *by.entry(k).or_default() += 1;
    }
    by.iter().map(|(k, n)| format!("{k}={n}")).collect::<Vec<_>>().join(", ")
}

fn list_lines(name: &str, rows: &[Value], count_by: Option<&str>, out: &mut Vec<String>) {
    out.push(format!("{name}: list of {}", rows.len()));
    if let Some(f) = count_by.filter(|_| !rows.is_empty()) {
        out.push(format!("  by {f}: {}", counts(rows, f)));
    }
    if let Some(first) = rows.first() {
        out.push(format!("  sample: {first}"));
    }
}

/// The shape of an already-masked answer.
pub fn shape(v: &Value, count_by: Option<&str>) -> String {
    let mut out = Vec::new();
    match v {
        Value::Object(m) => {
            out.push(format!("keys: {}", m.keys().cloned().collect::<Vec<_>>().join(", ")));
            for (k, x) in m {
                match x {
                    Value::Array(a) => list_lines(k, a, count_by, &mut out),
                    Value::Object(o) => out.push(format!("{k}: object, keys: {}", o.keys().cloned().collect::<Vec<_>>().join(", "))),
                    scalar => out.push(format!("{k}: {} = {scalar}", kind(scalar))),
                }
            }
        }
        Value::Array(a) => list_lines("(top level)", a, count_by, &mut out),
        scalar => out.push(format!("{}: {scalar}", kind(scalar))),
    }
    out.join("\n")
}

/// Call the route and render the answer, masked.
pub async fn get(c: &DeskClient, route: &str, post: Option<&str>, count_by: Option<&str>, raw: bool) -> Result<String> {
    let path = if route.starts_with('/') { route.to_string() } else { format!("/{route}") };
    let mut v = match post {
        Some(b) => {
            let body: Value = serde_json::from_str(b).context("--post is not JSON")?;
            c.post(&path, body).await?
        }
        None => c.get(&path).await?,
    };
    mask(&mut v, c.token());
    Ok(if raw { serde_json::to_string_pretty(&v)? } else { shape(&v, count_by) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::random_hex;
    use axum::{routing::get as aget, Json, Router};
    use std::time::Duration;

    #[test]
    fn shape_lists_keys_lengths_one_sample_and_counts() {
        let v = json!({"items": [{"k": "a", "n": 1}, {"k": "b"}, {"k": "a"}], "settings": {"x": 1}, "ok": true});
        let s = shape(&v, Some("k"));
        assert!(s.contains("keys: items, ok, settings"), "{s}");
        assert!(s.contains("items: list of 3"), "{s}");
        assert!(s.contains("by k: a=2, b=1"), "{s}");
        assert!(s.contains(r#"sample: {"k":"a","n":1}"#), "{s}");
        assert!(s.contains("settings: object, keys: x") && s.contains("ok: bool = true"), "{s}");
    }

    #[test]
    fn a_secret_is_masked_by_key_and_by_value() {
        let mut v = json!({"a": {"refresh_token": "r1", "label": "w"}, "rows": [{"Authorization": "Bearer abc"}, {"note": "has hunter2 inside"}]});
        mask(&mut v, "hunter2");
        let s = v.to_string();
        assert!(!s.contains("r1") && !s.contains("abc") && !s.contains("hunter2"), "{s}");
        assert!(s.contains(r#""label":"w""#), "{s}");
    }

    /// A stub desk on an ephemeral port that answers one route with secrets in it.
    async fn stub(token: &str) -> u16 {
        let t = token.to_string();
        let app = Router::new().route(
            "/leaky",
            aget(move || {
                let t = t.clone();
                async move { Json(json!({"rows": [{"id": 1, "kind": "x"}, {"id": 2, "kind": "x"}], "connector": {"token": "pasted-secret", "label": "w"}, "echo": format!("Bearer {t}")})) }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.ok() });
        port
    }

    fn instance_dir(token: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("desk-get-{}", random_hex(4).unwrap()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("desk-token"), format!("{token}\n")).unwrap();
        d
    }

    #[tokio::test]
    async fn a_named_instance_is_reached_by_its_own_folder_and_port_and_prints_no_secret() {
        let dir = instance_dir("dev-instance-token");
        let port = stub("dev-instance-token").await;
        let c = DeskClient::for_instance(Duration::from_secs(5), Some(&dir), Some(port)).unwrap();
        let shaped = get(&c, "leaky", None, Some("kind"), false).await.unwrap();
        assert!(shaped.contains("rows: list of 2") && shaped.contains("by kind: x=2"), "{shaped}");
        let raw = get(&c, "/leaky", None, None, true).await.unwrap();
        for out in [&shaped, &raw] {
            assert!(!out.contains("dev-instance-token") && !out.contains("pasted-secret"), "{out}");
        }
        assert!(raw.contains("***") && raw.contains(r#""label": "w""#), "{raw}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn an_instance_with_no_token_file_is_an_error_not_a_generated_token() {
        let dir = std::env::temp_dir().join(format!("desk-get-none-{}", random_hex(4).unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(DeskClient::for_instance(Duration::from_secs(1), Some(&dir), Some(1)).is_err());
        assert!(!dir.join("desk-token").exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
