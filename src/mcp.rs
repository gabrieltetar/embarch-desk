//! `embarch-desk mcp`: the agent-facing tools, shipped in the plugin.
//!
//! Agents manage work items — look up what a session is on, put it on one,
//! create or link one, add a note to it, pull everything the desk holds
//! about it (`desk_work_item`). **No tool approves, discards
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
    /// The desk a new standalone item belongs to (name or id, or "none"). Ask the user; it is required once desks exist.
    #[serde(default)]
    pub desk: Option<String>,
    /// True when the user says this session is not tracked work: it stops counting and stops being asked.
    #[serde(default)]
    pub untracked: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LineParams {
    /// The desk session id given in this session's context.
    pub session_id: String,
    /// One short line: what was done, found or decided, concretely. No issue key needed.
    pub text: String,
    /// done (the default: a piece of work finished), finding (something learned), decision (a choice made, and why), handoff (what the next session needs to know), todo (something left to do).
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateParams {
    /// A Linear issue key (`ABC-123`).
    #[serde(default)]
    pub issue_key: Option<String>,
    /// A title for a standalone work item.
    #[serde(default)]
    pub title: Option<String>,
    /// The desk a standalone item belongs to (name or id, or "none") — ask the user.
    #[serde(default)]
    pub desk: Option<String>,
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

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// A Linear issue key, e.g. ABC-123.
    pub key: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Words in the issue title, or an issue key.
    pub query: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    /// A Linear issue key, e.g. ABC-123.
    pub key: String,
    /// The workflow state's name as the team spells it, e.g. "In Review".
    pub status: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommentDraftParams {
    /// The issue the comment goes on.
    pub issue_key: String,
    /// The comment, in Markdown, as it should be posted.
    pub body: String,
    /// The desk session id from this session's context, so the draft shows on its card.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct IssueDraftParams {
    /// The Linear team key the issue goes to, e.g. ABC. Omit it to file into the open desk's Linear project.
    #[serde(default)]
    pub team_key: Option<String>,
    pub title: String,
    /// The description, in Markdown.
    #[serde(default)]
    pub description: String,
    /// The desk session id from this session's context.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RangeParams {
    /// First day, YYYY-MM-DD. Default: this week's Monday.
    #[serde(default)]
    pub from: Option<String>,
    /// Last day, YYYY-MM-DD. Default: today.
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TimerParams {
    /// An existing work item id.
    #[serde(default)]
    pub item_id: Option<String>,
    /// A Linear issue key; its card is found or created.
    #[serde(default)]
    pub issue_key: Option<String>,
    /// A title for a new standalone work item (a call, a meeting, bench work).
    #[serde(default)]
    pub new_title: Option<String>,
    /// The desk a new standalone item belongs to (name or id, or "none") — ask the user.
    #[serde(default)]
    pub desk: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct OpenParams {
    /// The desk's name or id; "none" opens no desk (a bare firmware repo, nothing scoped).
    pub desk: String,
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
        // Two routers, one per impl block, merged once here; the handler reads the field.
        Self { tool_router: Self::tool_router() + Self::linear_router(), client: Arc::new(client) }
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
        let body = json!({ "item_id": p.item_id, "issue_key": p.issue_key, "new_title": p.new_title, "desk": p.desk, "untracked": p.untracked });
        match self.client.post(&format!("/sessions/{}/assign", p.session_id), body).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Add a note to this session's work item: one short line, unasked, whenever something worth keeping happens — kind done (the default) for a piece of work finished without a commit, finding, decision, handoff or todo. Too many rather than too few; the user cuts. Commits and memory-file writes are added automatically; do not repeat them. The day's notes become the Harvest note.")]
    async fn desk_add_line(&self, Parameters(p): Parameters<LineParams>) -> Result<CallToolResult, McpError> {
        match self.client.post(&format!("/sessions/{}/line", p.session_id), json!({ "text": p.text, "kind": p.kind })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Everything the desk holds about this session's work item: the Linear issue with its description and latest comments, PRs, sessions, notes (each memory file's newest state, the latest lines) and bench activity, newest first, cut at 16,000 characters. Call it when starting on the item or whenever you need its context. Use the desk session id from this session's context.")]
    async fn desk_work_item(&self, Parameters(p): Parameters<SessionParams>) -> Result<CallToolResult, McpError> {
        let sess = match self.client.get(&format!("/sessions/{}", p.session_id)).await {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let Some(item) = sess["work_item"]["id"].as_str() else {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "This session is not on a work item yet. Ask the user which one it is and call desk_set_work_item.",
            )]));
        };
        match self.client.get(&format!("/items/{}/brief", crate::uri::encode(item))).await {
            Ok(v) => Ok(CallToolResult::success(vec![ContentBlock::text(v["text"].as_str().unwrap_or_default())])),
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
        match self.client.post("/items", json!({ "issue_key": p.issue_key, "title": p.title, "desk": p.desk })).await {
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

#[tool_router(router = linear_router, vis = "pub")]
impl DeskMcp {
    #[tool(description = "Read a Linear issue: title, state, team, project, assignee, labels, description and its latest comments. Works across every connected Linear workspace.")]
    async fn linear_get_issue(&self, Parameters(p): Parameters<KeyParams>) -> Result<CallToolResult, McpError> {
        let key = crate::uri::encode(p.key.trim());
        match self.client.get(&format!("/linear/issue/{key}")).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "The user's open Linear issues (assigned to them, not done or canceled), in every connected workspace, most recently updated first.")]
    async fn linear_my_issues(&self) -> Result<CallToolResult, McpError> {
        match self.client.get("/linear/mine").await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Find Linear issues by words in their title, or by key.")]
    async fn linear_search(&self, Parameters(p): Parameters<QueryParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/linear/search", json!({ "query": p.query })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Move a Linear issue to another workflow state by name (\"In Progress\", \"In Review\"). A status move goes straight through; the error lists the team's states if the name is wrong.")]
    async fn linear_set_status(&self, Parameters(p): Parameters<StatusParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/linear/status", json!({ "key": p.key, "status": p.status })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Propose a comment on a Linear issue. Nothing is posted: the draft waits on the user's Desk tab, where they edit it if they want and press Send. Tell the user it is waiting there.")]
    async fn linear_draft_comment(&self, Parameters(p): Parameters<CommentDraftParams>) -> Result<CallToolResult, McpError> {
        let body = json!({ "kind": "linear_comment", "issue_key": p.issue_key, "body": p.body, "session_id": p.session_id });
        match self.client.post("/drafts", body).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Propose a new Linear issue. Nothing is created: the draft waits on the user's Desk tab for their Send. If this session's work item has no issue yet, the new issue is attached to it once sent.")]
    async fn linear_draft_issue(&self, Parameters(p): Parameters<IssueDraftParams>) -> Result<CallToolResult, McpError> {
        let body = json!({ "kind": "linear_issue", "team_key": p.team_key, "title": p.title, "body": p.description, "session_id": p.session_id });
        match self.client.post("/drafts", body).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "The user's Harvest time entries between two dates (default: this week), with the total. Read-only: time is never logged from a session — it is captured on the work item's tile and the user sends it with a check mark on the Desk tab.")]
    async fn harvest_entries(&self, Parameters(p): Parameters<RangeParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/harvest/entries", json!({ "from": p.from, "to": p.to })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "The Harvest projects and tasks the user can log to, across every account.")]
    async fn harvest_projects(&self) -> Result<CallToolResult, McpError> {
        match self.client.get("/harvest/targets").await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Start the desk's manual timer on a work item, for work away from Claude Code — the bench, a call, a meeting (\"start tracking X\"). Starting one stops the running one. Its time lands on the item's tile like a session's.")]
    async fn desk_timer_start(&self, Parameters(p): Parameters<TimerParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/timer/start", json!({ "item_id": p.item_id, "issue_key": p.issue_key, "new_title": p.new_title, "desk": p.desk })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "The desks — one per project: its Linear project, Harvest project and task, GitHub repos, firmware repos and saved bench — and which one is open.")]
    async fn desk_list_desks(&self) -> Result<CallToolResult, McpError> {
        match self.client.get("/desks").await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Open a desk suite-wide, only when the user says to (\"switch to the X desk\", or yes to the desk's switch question): the UI, the agents' Linear and GitHub scope and embarch-api's default build target follow it; its saved bench is proposed in the UI, never applied by this tool. desk \"none\" opens no desk.")]
    async fn desk_open(&self, Parameters(p): Parameters<OpenParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/open", json!({ "desk": p.desk })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Stop the desk's manual timer (\"stop tracking\").")]
    async fn desk_timer_stop(&self) -> Result<CallToolResult, McpError> {
        match self.client.post("/timer/stop", json!({})).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DeskMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "embarch-desk: the one access point to Linear and Harvest, and the work item this session's \
             time and notes belong to. desk_work_item returns everything the desk holds about the session's \
             item. Read Linear with linear_get_issue, linear_my_issues and linear_search; \
             move a status with linear_set_status; propose a comment or an issue with linear_draft_comment or \
             linear_draft_issue — they wait for the user's Send on the Desk tab. Read Harvest with \
             harvest_entries and harvest_projects; time is never logged from a session: add notes with \
             desk_add_line and the user sends the tile. desk_timer_start/stop track work away from Claude. \
             The session's context carries its desk session id; when the desk asks which work item a session \
             is on, ask the user and call desk_set_work_item.",
        )
    }
}

pub async fn serve() -> anyhow::Result<()> {
    let server = DeskMcp::new(DeskClient::new(Duration::from_secs(120))?);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pull_is_a_session_tool_and_a_note_takes_a_kind() {
        let tools = DeskMcp::tool_router().list_all();
        let tool = |name: &str| tools.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no tool {name}"));
        let pull = serde_json::to_value(&tool("desk_work_item").input_schema).unwrap();
        assert_eq!(pull["required"], json!(["session_id"]));
        let line = serde_json::to_value(&tool("desk_add_line").input_schema).unwrap();
        assert!(line["properties"]["kind"].is_object(), "{line}");
        assert_eq!(line["required"], json!(["session_id", "text"]));
    }
}
