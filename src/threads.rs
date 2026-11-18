//! A pull request's review discussion, grouped for reading: inline threads
//! root-then-replies, the review bodies with their state, and a mark on each
//! thread the PR's author has not answered. Pure over GitHub's JSON, so the
//! tests feed it synthetic reviews and never a real PR.

use serde_json::{json, Value};

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}

fn who(v: &Value) -> &str {
    v["user"]["login"].as_str().unwrap_or("ghost")
}

/// One inline comment, kept to what a reader acts on.
fn comment(c: &Value) -> Value {
    json!({ "id": c["id"], "author": who(c), "at": s(c, "created_at"), "body": s(c, "body") })
}

/// Group `comments` (GitHub's review comments, any order) into threads and
/// pair them with `reviews`. A thread is awaiting the author when its latest
/// comment is not the PR author's; `awaiting_only` keeps just those.
pub fn build(repo: &str, number: u64, pr_author: &str, comments: &[Value], reviews: &[Value], awaiting_only: bool) -> Value {
    let mut sorted: Vec<&Value> = comments.iter().collect();
    sorted.sort_by(|a, b| (s(a, "created_at"), a["id"].as_u64()).cmp(&(s(b, "created_at"), b["id"].as_u64())));
    // A reply points at its root; GitHub keeps every reply on the root, but
    // follow a chain anyway so a reply to a reply still lands in its thread.
    let mut root_of: std::collections::HashMap<u64, u64> = Default::default();
    let mut order: Vec<u64> = vec![];
    let mut members: std::collections::HashMap<u64, Vec<&Value>> = Default::default();
    for c in &sorted {
        let Some(id) = c["id"].as_u64() else { continue };
        let root = c["in_reply_to_id"].as_u64().map(|p| *root_of.get(&p).unwrap_or(&p)).unwrap_or(id);
        root_of.insert(id, root);
        if !members.contains_key(&root) {
            order.push(root);
        }
        members.entry(root).or_default().push(c);
    }
    let mut threads = vec![];
    for root in order {
        let list = &members[&root];
        let first = list[0];
        let last = list[list.len() - 1];
        let awaiting = who(last) != pr_author;
        if awaiting_only && !awaiting {
            continue;
        }
        let line = first["line"].as_u64().or_else(|| first["original_line"].as_u64());
        threads.push(json!({
            "path": s(first, "path"),
            "line": line,
            "outdated": first["line"].is_null(),
            "root": comment(first),
            "replies": list[1..].iter().map(|c| comment(c)).collect::<Vec<_>>(),
            "awaiting_author": awaiting,
        }));
    }
    let mut revs: Vec<&Value> = reviews.iter().filter(|r| s(r, "state") != "PENDING").collect();
    revs.sort_by(|a, b| (s(a, "submitted_at"), a["id"].as_u64()).cmp(&(s(b, "submitted_at"), b["id"].as_u64())));
    let awaiting_n = threads.iter().filter(|t| t["awaiting_author"] == true).count();
    json!({
        "repo": repo,
        "number": number,
        "author": pr_author,
        "reviews": revs.iter().map(|r| json!({
            "id": r["id"], "author": who(r), "state": s(r, "state").to_lowercase(),
            "at": s(r, "submitted_at"), "body": s(r, "body"),
        })).collect::<Vec<_>>(),
        "threads": threads,
        "threads_total": threads.len(),
        "threads_awaiting_author": awaiting_n,
        "comments_total": comments.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u64, reply: Option<u64>, user: &str, at: &str, path: &str, line: Option<u64>) -> Value {
        json!({ "id": id, "in_reply_to_id": reply, "user": { "login": user }, "created_at": at,
                "path": path, "line": line, "original_line": 3, "body": format!("c{id}") })
    }

    #[test]
    fn replies_group_under_their_root_and_the_unanswered_thread_is_marked() {
        let comments = vec![
            c(12, Some(11), "owner", "2026-01-01T10:00:00Z", "a.rs", Some(7)),
            c(11, None, "rev", "2026-01-01T09:00:00Z", "a.rs", Some(7)),
            c(21, None, "rev", "2026-01-01T09:30:00Z", "b.rs", None),
            c(13, Some(11), "rev", "2026-01-01T11:00:00Z", "a.rs", Some(7)),
            c(31, None, "rev", "2026-01-01T09:40:00Z", "c.rs", Some(1)),
            c(32, Some(31), "owner", "2026-01-01T09:50:00Z", "c.rs", Some(1)),
        ];
        let v = build("o/r", 5, "owner", &comments, &[], false);
        let t = v["threads"].as_array().unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0]["root"]["id"], 11);
        assert_eq!(t[0]["replies"].as_array().unwrap().len(), 2);
        assert_eq!(t[0]["replies"][1]["author"], "rev");
        assert_eq!(t[0]["awaiting_author"], true, "reviewer spoke last");
        assert_eq!(t[0]["line"], 7);
        assert_eq!(t[1]["line"], 3, "an outdated thread falls back to its original line");
        assert_eq!(t[1]["outdated"], true);
        assert_eq!(t[2]["awaiting_author"], false, "the author replied last");
        assert_eq!(v["threads_awaiting_author"], 2);
        let only = build("o/r", 5, "owner", &comments, &[], true);
        assert_eq!(only["threads"].as_array().unwrap().len(), 2);
        assert_eq!(only["comments_total"], 6);
    }

    #[test]
    fn reviews_come_oldest_first_with_a_lowercase_state_and_no_pending_one() {
        let reviews = vec![
            json!({ "id": 2, "user": { "login": "b" }, "state": "APPROVED", "submitted_at": "2026-01-02T00:00:00Z", "body": "" }),
            json!({ "id": 1, "user": { "login": "a" }, "state": "CHANGES_REQUESTED", "submitted_at": "2026-01-01T00:00:00Z", "body": "fix" }),
            json!({ "id": 3, "user": { "login": "c" }, "state": "PENDING", "submitted_at": null, "body": "draft" }),
        ];
        let v = build("o/r", 5, "owner", &[], &reviews, false);
        let r = v["reviews"].as_array().unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["state"], "changes_requested");
        assert_eq!(r[1]["state"], "approved");
    }
}
