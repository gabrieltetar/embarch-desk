//! embarch-desk: the service behind embarch-ui's Desk tab, the hook every
//! Claude Code session runs, and the MCP server agents use — one binary.
//! embarch-doc/embarch-desk/spec.md is current truth.

mod claude;
mod brief;
mod client;
mod connectors;
mod db;
mod events;
mod github;
mod harvest;
mod hook;
mod linear;
mod linear_mcp;
mod localrepo;
mod mcp;
mod memory;
mod paths;
mod promptu;
mod refs;
mod server;
mod service;
mod store;
mod suite;
mod uri;
mod worktree;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "embarch-desk", version, about = "Work items, connectors and time tiles for the EmbArch Desk tab")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the desk (what the service runs).
    Serve,
    /// Forward a Claude Code hook event on stdin to the desk (what the plugin runs).
    Hook,
    /// Serve the agent-facing tools over MCP stdio (what the plugin runs).
    Mcp,
    /// Install, remove or show the systemd user service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Add a connector; the token is read from stdin so it stays out of shell history.
    Connect {
        /// linear, harvest or github
        kind: String,
        /// A name for this account, e.g. the workspace
        label: String,
        /// Linear only: sign in through Linear's MCP server in the browser
        /// instead of reading a key, for a workspace that gives you none.
        #[arg(long)]
        mcp: bool,
    },
    /// Remove a connector.
    Disconnect { kind: String, label: String },
    /// Show or change the paths that are never tracked.
    Ignore {
        #[command(subcommand)]
        action: IgnoreAction,
    },
    /// Is the desk up, and what does it see?
    Status,
    /// List the desks, the one the UI shows (*), and inside a Claude Code
    /// session the one it works on.
    Desks,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the service. With `--instance NAME`, a second desk beside the
    /// first: `embarch-desk-NAME.service`, its own port, config and data
    /// folders, and a login and `gh` folder of its own.
    Install {
        #[arg(long)]
        instance: Option<String>,
        /// The instance's port (required with --instance).
        #[arg(long)]
        port: Option<u16>,
        /// CLAUDE_CONFIG_DIR for the `claude -p` calls the desk makes.
        #[arg(long)]
        claude_config_dir: Option<String>,
        /// GH_CONFIG_DIR for the `gh` calls the desk makes.
        #[arg(long)]
        gh_config_dir: Option<String>,
        /// Capture no time: no tiles, no hand, no timer.
        #[arg(long)]
        no_time: bool,
        /// Agent issue and comment tools send at once instead of drafting.
        #[arg(long)]
        direct_writes: bool,
        /// The folder of a suite's sibling clones: Start work makes a folder
        /// per issue there, picking the repo by the issue's Project.
        #[arg(long)]
        suite_root: Option<String>,
        /// The repo in that folder every issue also gets a worktree of.
        #[arg(long, requires = "suite_root")]
        suite_docs: Option<String>,
    },
    Uninstall {
        #[arg(long)]
        instance: Option<String>,
    },
    Status {
        #[arg(long)]
        instance: Option<String>,
    },
}

#[derive(Subcommand)]
enum IgnoreAction {
    List,
    Add { path: String },
    Remove { path: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hook => {
            hook::run();
            Ok(())
        }
        Cmd::Service { action } => match action {
            ServiceAction::Install { instance, port, claude_config_dir, gh_config_dir, no_time, direct_writes, suite_root, suite_docs } => {
                service::install(service::Instance::new(instance, port, claude_config_dir, gh_config_dir, no_time, direct_writes, (suite_root, suite_docs))?)
            }
            ServiceAction::Uninstall { instance } => service::uninstall(instance.as_deref()),
            ServiceAction::Status { instance } => service::status(instance.as_deref()),
        },
        Cmd::Mcp => runtime()?.block_on(mcp::serve()),
        Cmd::Serve => runtime()?.block_on(serve()),
        Cmd::Connect { kind, label, mcp: true } => {
            anyhow::ensure!(kind == "linear", "--mcp signs in to Linear only");
            runtime()?.block_on(sign_in_through_mcp(&label))
        }
        Cmd::Connect { kind, label, mcp: false } => {
            let mut token = String::new();
            std::io::stdin().read_to_string(&mut token)?;
            runtime()?.block_on(async {
                let c = client::DeskClient::new(Duration::from_secs(60))?;
                let v = c.post("/connectors", serde_json::json!({ "kind": kind, "label": label, "token": token.trim() })).await?;
                println!("{kind}/{label} connected: {}", v["detail"].as_str().unwrap_or(""));
                Ok(())
            })
        }
        Cmd::Disconnect { kind, label } => runtime()?.block_on(async {
            let c = client::DeskClient::new(Duration::from_secs(10))?;
            let v = c.call(reqwest::Method::DELETE, &format!("/connectors/{kind}/{label}"), None).await?;
            println!("removed: {}", v["removed"]);
            Ok(())
        }),
        Cmd::Ignore { action } => runtime()?.block_on(async {
            let c = client::DeskClient::new(Duration::from_secs(10))?;
            let mut paths: Vec<String> = c.get("/ignore").await?["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect();
            match action {
                IgnoreAction::List => {}
                IgnoreAction::Add { path } => {
                    let abs = std::fs::canonicalize(&path).with_context(|| format!("{path} does not exist"))?;
                    paths.push(abs.display().to_string());
                    paths.sort();
                    paths.dedup();
                }
                IgnoreAction::Remove { path } => paths.retain(|p| p != path.trim_end_matches('/')),
            }
            let v = c.call(reqwest::Method::PUT, "/ignore", Some(serde_json::json!({ "paths": paths }))).await?;
            for p in v["paths"].as_array().into_iter().flatten() {
                println!("{}", p.as_str().unwrap_or_default());
            }
            Ok(())
        }),
        Cmd::Desks => runtime()?.block_on(async {
            let c = client::DeskClient::new(Duration::from_secs(10))?;
            let v = c.get("/desks").await?;
            let open = v["open"].as_str().unwrap_or("");
            for d in v["desks"].as_array().into_iter().flatten() {
                let mark = if d["id"].as_str() == Some(open) { "*" } else { " " };
                println!(
                    "{mark} {}  {}  linear: {}  repos: {}",
                    d["id"].as_str().unwrap_or(""),
                    d["name"].as_str().unwrap_or(""),
                    match d["linear"]["project_name"].as_str() {
                        Some(p) if !p.is_empty() => p.to_string(),
                        _ => d["linear"]["team_key"].as_str().map(|k| format!("team {k}")).unwrap_or_else(|| "-".into()),
                    },
                    d["firmware_repos"].as_array().map(|r| r.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
                );
            }
            if open.is_empty() {
                println!("the UI shows no desk");
            }
            if let Some(s) = v.get("session") {
                let name = s.as_str().and_then(|k| v["desks"].as_array()?.iter().find(|d| d["id"].as_str() == Some(k))?["name"].as_str());
                println!("this session works on: {}", name.unwrap_or("no desk"));
            }
            Ok(())
        }),
        Cmd::Status => runtime()?.block_on(async {
            let c = client::DeskClient::new(Duration::from_secs(10))?;
            let v = c.get("/view").await?;
            println!("desk up at {}", paths::base_url());
            println!("cards: {}  sessions waiting for a work item: {}", v["cards"].as_array().map_or(0, Vec::len), v["sessions_waiting"]);
            for s in v["connectors"].as_array().into_iter().flatten() {
                println!("  {}/{}: {} {}", s["kind"].as_str().unwrap_or(""), s["label"].as_str().unwrap_or(""), if s["ok"] == true { "ok" } else { "FAIL" }, s["detail"].as_str().unwrap_or(""));
            }
            Ok(())
        }),
    }
}

/// Hand the user the sign-in link, then wait for the browser to come back to
/// the desk (decision 68).
async fn sign_in_through_mcp(label: &str) -> Result<()> {
    let c = client::DeskClient::new(Duration::from_secs(60))?;
    let v = c.post("/linear/signin", serde_json::json!({ "label": label })).await?;
    let state = v["state"].as_str().unwrap_or_default();
    println!("Open this link, sign in to Linear and pick the workspace for {label}:\n\n{}\n", v["url"].as_str().unwrap_or_default());
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        // A 404 once the link's 10 minutes are up ends the wait.
        let s = c.get(&format!("/linear/signin/{state}")).await?;
        match s["status"].as_str() {
            Some("ok") => {
                println!("linear/{label} connected: {}", s["detail"].as_str().unwrap_or(""));
                return Ok(());
            }
            Some("failed") => anyhow::bail!("linear/{label} did not connect: {}", s["detail"].as_str().unwrap_or("")),
            _ => {}
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

async fn serve() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "embarch_desk=info".into()),
    ).init();
    let db = db::open_default()?;
    // An instance's unit states its settings, so a fresh data folder comes up
    // right and a changed unit takes effect on restart.
    for (var, apply) in [
        ("EMBARCH_DESK_TIME", (|st: &mut store::State, on| st.settings.time = on) as fn(&mut store::State, bool)),
        ("EMBARCH_DESK_DIRECT_WRITES", |st, on| st.settings.direct_writes = on),
    ] {
        if let Some(on) = std::env::var(var).ok().and_then(|v| match v.as_str() {
            "1" | "on" => Some(true),
            "0" | "off" => Some(false),
            _ => None,
        }) {
            db.write(|st| apply(st, on));
        }
    }
    let refs = refs::Store::open(db.dir());
    let app = Arc::new(server::App {
        db,
        refs,
        token: paths::desk_token()?,
        cache: Mutex::new(server::Cache { wsl_distro: server::detect_wsl_distro(), ..Default::default() }),
        wsl_distro: server::detect_wsl_distro(),
        window_days: std::env::var("EMBARCH_DESK_WINDOW_DAYS").ok().and_then(|d| d.parse().ok()).unwrap_or(7),
        model: std::env::var("EMBARCH_DESK_MODEL").unwrap_or_else(|_| claude::DEFAULT_MODEL.into()),
    });
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], paths::port()));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr} — is another desk already running?"))?;
    tracing::info!("embarch-desk listening on http://{addr}");
    server::publish_desks(&app);
    let import_app = app.clone();
    tokio::spawn(async move { server::import_memories(&import_app).await });
    tokio::spawn(server::flush_loop(app.clone()));
    tokio::spawn(server::sync_loop(app.clone()));
    tokio::spawn(server::refs_loop(app.clone()));
    let stop = server::save_on_stop(app.clone())?;
    axum::serve(listener, server::router(app)).with_graceful_shutdown(stop).await?;
    Ok(())
}
