//! An issue's files (decision 81): the Linear upload links in its description
//! and comments and its attachment records are listed by `linear_get_issue`,
//! and `linear_get_file` fetches one of them at call time with the workspace's
//! credentials. **Only a link the issue itself lists is fetched**, so the tool
//! is never a general fetch through the owner's tokens, and the credential goes
//! only to Linear's own upload host, never to a third party an attachment
//! record happens to name.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// Linear's upload host: the one that needs the workspace's credentials.
pub const UPLOAD_HOST: &str = "uploads.linear.app";
/// Largest file the tool fetches (10 MiB). A bigger one is refused naming its size.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;
/// Largest text returned in the answer itself (200 KB); a longer text file is
/// written under the data folder like any other file, so it does not fill the context.
pub const TEXT_INLINE_MAX: usize = 200_000;

/// The files an issue lists. `description` and `comments` (each with `id`,
/// `author` and `body`) are searched for upload links, `attachments` (each with
/// `title` and `url`) are taken as they are. Each row says where it was found.
pub fn files_of(description: Option<&str>, comments: &[Value], attachments: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    let mut add = |url: &str, title: Option<&str>, found: String| {
        if !out.iter().any(|f| f["url"] == url) {
            out.push(json!({ "url": url, "title": title, "found": found }));
        }
    };
    for u in upload_links(description.unwrap_or_default()) {
        add(&u, None, "description".to_string());
    }
    for c in comments {
        let found = format!("comment {} by {}", c["id"].as_str().unwrap_or("?"), c["author"].as_str().unwrap_or("?"));
        for u in upload_links(c["body"].as_str().unwrap_or_default()) {
            add(&u, None, found.clone());
        }
    }
    for a in attachments {
        if let Some(u) = a["url"].as_str().filter(|u| !u.is_empty()) {
            add(u, a["title"].as_str(), "attachment".to_string());
        }
    }
    out
}

/// Every Linear upload link in a piece of markdown, in order, once each.
pub fn upload_links(text: &str) -> Vec<String> {
    let prefix = format!("https://{UPLOAD_HOST}/");
    let mut out: Vec<String> = vec![];
    let mut rest = text;
    while let Some(at) = rest.find(&prefix) {
        let tail = &rest[at..];
        let end = tail.find(|c: char| c.is_whitespace() || ")]>\"'<`".contains(c)).unwrap_or(tail.len());
        let link = tail[..end].trim_end_matches(['.', ',', ';']);
        if link.len() > prefix.len() && !out.iter().any(|l| l == link) {
            out.push(link.to_string());
        }
        rest = &tail[end.max(1)..];
    }
    out
}

/// The listed row for `url`, or the refusal naming the issue.
pub fn require_listed<'a>(issue: &'a Value, key: &str, url: &str) -> Result<&'a Value> {
    issue["files"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["url"] == url))
        .ok_or_else(|| anyhow!("{url} is not a file {key} lists; linear_get_issue shows the issue's `files`, and this tool reads only those"))
}

/// The authority of an `https://` URL, lowercased; an error for anything the
/// tool will not fetch: another scheme, a login in the URL, a port, `localhost`
/// or an IP address (an attachment record is free text an issue's author wrote).
pub fn host_of(url: &str) -> Result<String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| anyhow!("{url} is not an https link; the tool fetches only https"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default().to_ascii_lowercase();
    let literal = host.chars().all(|c| c.is_ascii_digit() || c == '.') || host.starts_with('[');
    if host.is_empty() || host.contains(['@', ':']) || literal || host == "localhost" || host.ends_with(".localhost") {
        bail!("{url} names a host the tool will not fetch from (a login, a port, an IP address or localhost)");
    }
    Ok(host)
}

/// What a fetch got.
#[derive(Debug)]
pub struct Fetched {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// A response that was not a success, kept apart so a backend can try another way.
#[derive(Debug)]
pub struct HttpStatus(pub u16);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}

impl std::error::Error for HttpStatus {}

fn too_big(size: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("the file is {size} bytes, over the {MAX_BYTES}-byte cap; not fetched")
}

/// Fetch `url`, sending `auth` (header name and value) when given, and stop
/// at `MAX_BYTES`. Redirects drop the credential on a change of host.
pub async fn download(url: &str, auth: Option<(&str, String)>) -> Result<Fetched> {
    let mut req = crate::connectors::http().get(url);
    if let Some((name, value)) = auth {
        req = req.header(name, value);
    }
    let mut resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(HttpStatus(status.as_u16()).into());
    }
    if let Some(n) = resp.content_length().filter(|n| *n > MAX_BYTES as u64) {
        return Err(too_big(n));
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_BYTES {
            return Err(too_big(format!("over {MAX_BYTES}")));
        }
    }
    Ok(Fetched { mime, bytes })
}

/// What the tool answers with.
#[derive(Debug, PartialEq)]
pub enum Read {
    Image { mime: String, data: String, name: String, size: usize },
    Text { text: String, name: String, size: usize },
    Saved { path: String, mime: String, name: String, size: usize },
}

impl Read {
    pub fn to_json(&self) -> Value {
        match self {
            Read::Image { mime, data, name, size } => json!({ "kind": "image", "mime": mime, "data": data, "name": name, "size": size }),
            Read::Text { text, name, size } => json!({ "kind": "text", "text": text, "name": name, "size": size }),
            Read::Saved { path, mime, name, size } => json!({ "kind": "file", "path": path, "mime": mime, "name": name, "size": size }),
        }
    }
}

/// A file name that is safe on disk, from the last segment of the URL.
pub fn file_name(url: &str) -> String {
    let last = url.split(['?', '#']).next().unwrap_or_default().rsplit('/').next().unwrap_or_default();
    let clean: String = last.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    let clean = clean.trim_start_matches('.');
    if clean.is_empty() { "file".to_string() } else { clean.chars().take(100).collect() }
}

fn is_text(mime: &str, bytes: &[u8]) -> bool {
    let named = mime.starts_with("text/")
        || mime == "image/svg+xml"
        || mime.ends_with("json")
        || mime.ends_with("xml")
        || mime == "application/yaml"
        || mime == "application/toml";
    let unnamed = mime.is_empty() || mime == "application/octet-stream";
    named || (unnamed && !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok())
}

/// Sort a fetched file into what the session gets: an image as image content,
/// a text as text, anything else (and a text too long to return) written under
/// `dir`, `<dir>/<issue key>/<name>`, with its path.
pub fn deliver(dir: &std::path::Path, key: &str, url: &str, f: Fetched) -> Result<Read> {
    let (name, size) = (file_name(url), f.bytes.len());
    if f.mime.starts_with("image/") && f.mime != "image/svg+xml" {
        use base64::Engine;
        return Ok(Read::Image { mime: f.mime, data: base64::engine::general_purpose::STANDARD.encode(&f.bytes), name, size });
    }
    if is_text(&f.mime, &f.bytes) && size <= TEXT_INLINE_MAX {
        if let Ok(text) = String::from_utf8(f.bytes.clone()) {
            return Ok(Read::Text { text, name, size });
        }
    }
    let path = dir.join(key.replace(['/', '\\'], "_")).join(&name);
    crate::paths::write_private(&path, &f.bytes)?;
    Ok(Read::Saved { path: path.to_string_lossy().into_owned(), mime: f.mime, name, size })
}

/// The folder files are written to.
pub fn files_dir() -> Result<std::path::PathBuf> {
    Ok(crate::paths::data_dir()?.join("linear-files"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "https://uploads.linear.app/ws/1/2/aaa";
    const B: &str = "https://uploads.linear.app/ws/3/4/bbb.png";

    #[test]
    fn upload_links_come_out_of_markdown_once_each_without_its_punctuation() {
        let text = format!("see ![shot]({A}) and [file]({B}), again {A}. Also https://example.com/x.png");
        assert_eq!(upload_links(&text), vec![A.to_string(), B.to_string()]);
        assert!(upload_links("nothing https://uploads.linear.app/ here").is_empty());
    }

    #[test]
    fn a_file_is_listed_once_with_where_it_was_found() {
        let comments = vec![json!({ "id": "c1", "author": "Ana", "body": format!("![x]({B}) ![y]({A})") })];
        let atts = vec![json!({ "title": "Spec", "url": "https://github.com/o/r/pull/1" }), json!({ "title": "no url", "url": null })];
        let f = files_of(Some(&format!("![x]({A})")), &comments, &atts);
        let at = |i: usize| (f[i]["url"].as_str().unwrap().to_string(), f[i]["found"].as_str().unwrap().to_string());
        assert_eq!(f.len(), 3);
        assert_eq!(at(0), (A.to_string(), "description".to_string()));
        assert_eq!(at(1), (B.to_string(), "comment c1 by Ana".to_string()));
        assert_eq!(at(2), ("https://github.com/o/r/pull/1".to_string(), "attachment".to_string()));
        assert_eq!(f[2]["title"], "Spec");
    }

    #[test]
    fn a_link_the_issue_does_not_list_is_refused_naming_the_issue() {
        let issue = json!({ "files": files_of(Some(A), &[], &[]) });
        assert!(require_listed(&issue, "ABC-1", A).is_ok());
        let e = require_listed(&issue, "ABC-1", "https://example.com/secret").unwrap_err().to_string();
        assert!(e.contains("ABC-1") && e.contains("not a file"), "{e}");
        assert!(require_listed(&json!({}), "ABC-1", A).is_err());
    }

    #[test]
    fn only_a_public_https_host_is_fetched() {
        assert_eq!(host_of(A).unwrap(), UPLOAD_HOST);
        for bad in ["http://uploads.linear.app/x", "https://127.0.0.1/x", "https://localhost/x", "https://u:p@host/x", "https://host:8080/x", "https://[::1]/x"] {
            assert!(host_of(bad).is_err(), "{bad}");
        }
    }

    fn dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("desk-files-{}", crate::paths::random_hex(4).unwrap()))
    }

    fn got(mime: &str, bytes: &[u8]) -> Fetched {
        Fetched { mime: mime.into(), bytes: bytes.to_vec() }
    }

    #[test]
    fn an_image_is_image_content_a_text_is_text_and_the_rest_is_a_path_under_the_data_folder() {
        let d = dir();
        match deliver(&d, "ABC-1", B, got("image/png", &[1, 2, 3])).unwrap() {
            Read::Image { mime, data, name, size } => assert_eq!((mime.as_str(), data.as_str(), name.as_str(), size), ("image/png", "AQID", "bbb.png", 3)),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            deliver(&d, "ABC-1", A, got("text/plain", b"hello")).unwrap(),
            Read::Text { text: "hello".into(), name: "aaa".into(), size: 5 }
        );
        assert!(matches!(deliver(&d, "ABC-1", A, got("application/octet-stream", b"log line")).unwrap(), Read::Text { .. }));
        match deliver(&d, "ABC-1", "https://uploads.linear.app/w/1/spec.pdf?sig=1", got("application/pdf", b"%PDF\0")).unwrap() {
            Read::Saved { path, size, .. } => {
                assert_eq!(std::path::PathBuf::from(&path), d.join("ABC-1").join("spec.pdf"));
                assert_eq!((std::fs::read(&path).unwrap(), size), (b"%PDF\0".to_vec(), 5));
            }
            other => panic!("{other:?}"),
        }
        // A text too long to return is written, not returned.
        let long = vec![b'a'; TEXT_INLINE_MAX + 1];
        assert!(matches!(deliver(&d, "ABC-1", A, got("text/plain", &long)).unwrap(), Read::Saved { .. }));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_name_from_the_url_cannot_leave_the_folder() {
        assert_eq!(file_name("https://uploads.linear.app/w/..%2F..%2Fx"), "_2F.._2Fx");
        assert_eq!(file_name("https://uploads.linear.app/w/.."), "file");
        assert_eq!(file_name("https://uploads.linear.app/w/a b.png?x=1"), "a_b.png");
    }

    async fn serve(body: &'static [u8], mime: &'static str) -> String {
        let app = axum::Router::new().route("/f", axum::routing::get(move || async move { ([("content-type", mime)], body) }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn a_download_reads_the_body_and_its_type_and_a_missing_file_is_a_status() {
        let url = serve(b"abc", "text/plain; charset=utf-8").await;
        let f = download(&url, None).await.unwrap();
        assert_eq!((f.mime.as_str(), f.bytes.as_slice()), ("text/plain", b"abc".as_slice()));
        let e = download(&url.replace("/f", "/none"), None).await.unwrap_err();
        assert_eq!(e.downcast_ref::<HttpStatus>().map(|s| s.0), Some(404));
    }
}
