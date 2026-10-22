//! embarch-desk: the service behind embarch-ui's Desk tab, the hook every
//! Claude Code session runs, and the MCP server agents use — one binary.
//! embarch-doc/embarch-desk/spec.md is current truth.

mod claude;
mod client;
mod connectors;
mod db;
mod events;
mod github;
mod harvest;
mod hook;
mod linear;
mod localrepo;
mod mcp;
mod paths;
mod server;
mod service;
mod slack;
mod store;
mod uri;

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
        /// linear, harvest, github or slack
        kind: String,
        /// A name for this account, e.g. the workspace
        label: String,
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
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Status,
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
            ServiceAction::Install => service::install(),
            ServiceAction::Uninstall => service::uninstall(),
            ServiceAction::Status => service::status(),
        },
        Cmd::Mcp => runtime()?.block_on(mcp::serve()),
        Cmd::Serve => runtime()?.block_on(serve()),
        Cmd::Connect { kind, label } => {
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

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

async fn serve() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "embarch_desk=info".into()),
    ).init();
    let app = Arc::new(server::App {
        db: db::open_default()?,
        token: paths::desk_token()?,
        cache: Mutex::new(server::Cache::default()),
        wsl_distro: server::detect_wsl_distro(),
        window_days: std::env::var("EMBARCH_DESK_WINDOW_DAYS").ok().and_then(|d| d.parse().ok()).unwrap_or(7),
        model: std::env::var("EMBARCH_DESK_MODEL").unwrap_or_else(|_| claude::DEFAULT_MODEL.into()),
    });
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], paths::port()));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr} — is another desk already running?"))?;
    tracing::info!("embarch-desk listening on http://{addr}");
    tokio::spawn(server::flush_loop(app.clone()));
    tokio::spawn(server::sync_loop(app.clone()));
    let shutdown_app = app.clone();
    axum::serve(listener, server::router(app))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = shutdown_app.db.flush();
        })
        .await?;
    Ok(())
}
