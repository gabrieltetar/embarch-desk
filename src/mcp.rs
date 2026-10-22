//! `embarch-desk mcp`: the agent-facing tools, shipped in the plugin.
//!
//! Agents manage work items — look up what a session is on, put it on one,
//! create or link one, add a line to its tile. **No tool approves, discards
//! or archives anything**: ✓, ✕ and archive are the user's, on the Desk tab.
//!
//! Claude Code gives an MCP server no session identity, so every
//! session-scoped tool takes the `session_id` the desk's SessionStart hook
//! put into the session's context.

use crate::client::DeskClient;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct DeskMcp {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    client: Arc<DeskClient>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionParams {
    /// The desk session id given in this session's context ("desk session id `…`").
    pub session_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SetWorkItemParams {
    /// The desk session id given in this session's context.
    pub session_id: String,
    /// An existing work item's id (`w12`), from the list the desk gave or desk_list_items.
    #[serde(default)]
    pub item_id: Option<String>,
    /// A Linear issue key (`ABC-123`); its card is found or created.
    #[serde(default)]
    pub issue_key: Option<String>,
    /// A title for a new standalone work item with no Linear issue (a call, a meeting, support).
    #[serde(default)]
    pub new_title: Option<String>,
    /// True when the user says this session is not tracked work: it stops counting and stops being asked.
    #[serde(default)]
    pub untracked: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LineParams {
    /// The desk session id given in this session's context.
    pub session_id: String,
    /// One short past-tense line: what was found or done, concretely. No issue key needed.
    pub text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateParams {
    /// A Linear issue key (`ABC-123`).
    #[serde(default)]
    pub issue_key: Option<String>,
    /// A title for a standalone work item.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LinkParams {
    pub item_id: String,
    /// Attach this Linear issue to the work item.
    #[serde(default)]
    pub issue_key: Option<String>,
    /// The work item's local clone, for "open in VS Code".
    #[serde(default)]
    pub repo_path: Option<String>,
}

fn ok(v: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&v).unwrap_or_default())]))
}

fn err(e: anyhow::Error) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]))
}

#[tool_router]
impl DeskMcp {
    pub fn new(client: DeskClient) -> Self {
        Self { tool_router: Self::tool_router(), client: Arc::new(client) }
    }

    #[tool(description = "What this session is working on: its work item (Linear issue, repo) and today's time tile with its lines. Use the desk session id from this session's context.")]
    async fn desk_whoami(&self, Parameters(p): Parameters<SessionParams>) -> Result<CallToolResult, McpError> {
        match self.client.get(&format!("/sessions/{}", p.session_id)).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Put this session on a work item, after the user said which one: an existing item_id, a Linear issue_key (its card is found or created), or new_title for a standalone item. untracked: true when the user says this is not tracked work. Time the session already ran up moves onto the item.")]
    async fn desk_set_work_item(&self, Parameters(p): Parameters<SetWorkItemParams>) -> Result<CallToolResult, McpError> {
        let body = json!({ "item_id": p.item_id, "issue_key": p.issue_key, "new_title": p.new_title, "untracked": p.untracked });
        match self.client.post(&format!("/sessions/{}/assign", p.session_id), body).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Add one line to this session's time tile for today: a milestone with no commit — an investigation's finding, a flash or study result. Commits are added automatically; do not repeat them. Claude later turns the day's lines into the Harvest note.")]
    async fn desk_add_line(&self, Parameters(p): Parameters<LineParams>) -> Result<CallToolResult, McpError> {
        match self.client.post(&format!("/sessions/{}/line", p.session_id), json!({ "text": p.text })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "List the work items on the user's desk: id, title, Linear issue and state, PRs with CI, repo, today's time.")]
    async fn desk_list_items(&self) -> Result<CallToolResult, McpError> {
        match self.client.get("/view").await {
            Ok(v) => {
                let cards: Vec<Value> = v["cards"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| {
                        json!({
                            "id": c["id"], "title": c["title"],
                            "issue": c["issue"]["key"], "state": c["issue"]["state_name"],
                            "repo_path": c["repo_path"], "prs": c["prs"],
                            "today_hours": c["tiles"].as_array().and_then(|t| t.iter().find(|t| t["date"] == v["today"])).map(|t| t["hours"].clone()),
                        })
                    })
                    .collect();
                ok(json!({ "items": cards }))
            }
            Err(e) => err(e),
        }
    }

    #[tool(description = "Create a work item: from a Linear issue_key, or a standalone one from a title. Returns its id; an issue that already has a card returns that card.")]
    async fn desk_create_item(&self, Parameters(p): Parameters<CreateParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/items", json!({ "issue_key": p.issue_key, "title": p.title })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Link a work item to a Linear issue (issue_key) and/or set its local repo folder (repo_path).")]
    async fn desk_link(&self, Parameters(p): Parameters<LinkParams>) -> Result<CallToolResult, McpError> {
        match self.client.post(&format!("/items/{}/link", p.item_id), json!({ "issue_key": p.issue_key, "repo_path": p.repo_path })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }
}

#[tool_handler]
impl ServerHandler for DeskMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "embarch-desk: the work item this session's time and lines belong to. The session's context \
             carries its desk session id. When the desk asks which work item a session is on, ask the user \
             and call desk_set_work_item. Add a line with desk_add_line when a piece of work ends without a \
             commit. Approving or discarding time is the user's, on the Desk tab — no tool does it.",
        )
    }
}

pub async fn serve() -> anyhow::Result<()> {
    let server = DeskMcp::new(DeskClient::new(Duration::from_secs(120))?);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
