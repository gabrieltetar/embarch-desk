//! `embarch-desk mcp`: the agent-facing tools, shipped in the plugin.
//!
//! Agents manage work items — look up what a session is on, put it on one,
//! create or link one, add a note to it, pull everything the desk holds
//! about it (`desk_work_item`) — and add a standing note to the session's
//! desk (`desk_add_desk_note`, decision 74). **No tool approves, discards
//! or archives anything**: ✓, ✕ and archive are the user's, on the Desk tab.
//!
//! Claude Code gives an MCP server no session identity, so every
//! session-scoped tool takes the `session_id` the desk's SessionStart hook
//! put into the session's context.

use crate::client::DeskClient;
use crate::store::{desk_note_list, DeskNote};
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
    /// True for a machine's session, such as a fleet's listener: untracked, and left off the desk's session list.
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AddRepoParams {
    /// The desk session id from this session's context.
    pub session_id: String,
    /// The clone's folder name in the suite, such as `embarch-api`.
    pub repo: String,
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
    /// Another work item this note belongs to: its issue key (`ABC-123`) or desk item id, on any desk. Leave out for a note about this session's own item.
    #[serde(default)]
    pub item: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DeskNoteParams {
    /// The desk session id given in this session's context.
    pub session_id: String,
    /// One standing rule, as every future session in this desk's repos should read it: at most 500 characters.
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
pub struct IssueParams {
    /// A Linear issue key, e.g. ABC-123.
    pub key: String,
    /// How many of the newest comments to skip, to read older ones: the answer's `comments_note` gives the number. Default 0.
    #[serde(default)]
    pub skip: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Words in the issue title, or an issue key.
    pub query: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StateListParams {
    /// A team's key, e.g. EMB (linear_teams lists them).
    pub team_key: String,
    /// The workflow state's name as the team spells it, e.g. "Fleet Ready"; case is ignored.
    pub state: String,
    /// Narrow to one project, by name.
    #[serde(default)]
    pub project: Option<String>,
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
    /// The id of a comment on the issue (from linear_get_issue) this one answers: it is posted as a reply in that comment's thread.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// More comments, posted in order as replies under this one by the same Send: a message too long for one comment, split into a thread.
    #[serde(default)]
    pub replies: Vec<String>,
    /// The desk session id from this session's context, so the draft shows on its card.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EditDraftParams {
    /// The issue to change.
    pub issue_key: String,
    /// The new title. Omit it to keep the current one.
    #[serde(default)]
    pub title: Option<String>,
    /// The whole new description, in Markdown; it replaces the current one. Omit it to keep the current one.
    #[serde(default)]
    pub description: Option<String>,
    /// The desk session id from this session's context, so the draft shows on its card.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct IssueDraftParams {
    /// The Linear team key the issue goes to, e.g. ABC. Omit it to file into this session's desk's Linear project.
    #[serde(default)]
    pub team_key: Option<String>,
    pub title: String,
    /// The name of the Linear Project the issue goes in. Needs team_key.
    #[serde(default)]
    pub project: Option<String>,
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

/// The pull's text followed by the standing notes `GET /sessions/{sid}`
/// gives (decision 74): those the session heard at its start, repeated for
/// one whose start hook was lost. Unchanged when there are none.
fn with_desk_notes(text: &str, sess: &Value) -> String {
    let notes: Vec<DeskNote> = serde_json::from_value(sess["desk_notes"].clone()).unwrap_or_default();
    if notes.is_empty() {
        return text.to_string();
    }
    format!(
        "{}\n\n## Desk notes\nStanding notes for every session in this desk's repos, the ones this session was given at its \
         start; follow them.\n{}\n",
        text.trim_end(),
        desk_note_list(&notes)
    )
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
        let body = json!({ "item_id": p.item_id, "issue_key": p.issue_key, "new_title": p.new_title, "desk": p.desk, "untracked": p.untracked, "hidden": p.hidden });
        match self.client.post(&format!("/sessions/{}/assign", p.session_id), body).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "On a suite desk, give another repo a worktree on this session's branch, replacing its symlink in the issue's folder: use this before changing a repo that is still a symlink, and never edit through the symlink, which is the main clone. Repo is a clone's folder name, such as embarch-api. Answers with the worktree's path. Use the desk session id from this session's context.")]
    async fn desk_add_repo(&self, Parameters(p): Parameters<AddRepoParams>) -> Result<CallToolResult, McpError> {
        let sess = match self.client.get(&format!("/sessions/{}", p.session_id)).await {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let Some(item) = sess["work_item"]["id"].as_str() else {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "This session is not on a work item yet. If the user's opening prompt or a handoff holds an issue key of a connected team (linear_teams lists them; a lookalike in prose is not one), call desk_set_work_item with it and say which in one line. Only with no such key, ask the user which item it is and call desk_set_work_item.",
            )]));
        };
        match self.client.post(&format!("/items/{item}/repos"), json!({ "repo": p.repo })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Add a note to this session's work item: one short line, unasked, whenever something worth keeping happens — kind done (the default) for a piece of work finished without a commit, finding, decision, handoff or todo. Too many rather than too few; the user cuts. Commits and memory-file writes are added automatically; do not repeat them. The day's notes become the Harvest note. Set item (an issue key or desk item id) only when the note is about a different work item than this session's own, such as something learned about another issue while working on this one: it lands on that item's tile for today, with this session's id. An unknown item is an error and nothing is written.")]
    async fn desk_add_line(&self, Parameters(p): Parameters<LineParams>) -> Result<CallToolResult, McpError> {
        match self.client.post(&format!("/sessions/{}/line", p.session_id), json!({ "text": p.text, "kind": p.kind, "item": p.item })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Add a standing note to the desk this session belongs to: a working agreement that should hold for every future session in this desk's repos — the user said always, never, from now on or in this repo, or agreed when you asked. Never progress, findings or task state (that is desk_add_line). Never part of Harvest. One rule per note, at most 500 characters; a desk holds 4,000.")]
    async fn desk_add_desk_note(&self, Parameters(p): Parameters<DeskNoteParams>) -> Result<CallToolResult, McpError> {
        match self.client.post(&format!("/sessions/{}/desk-note", p.session_id), json!({ "text": p.text })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Everything the desk holds about this session's work item: the Linear issue with its description and latest comments, PRs, sessions, notes (each memory file's newest state, the latest lines) and bench activity, newest first, cut at 16,000 characters; then the standing notes of the desk this session was told of at its start. Call it when starting on the item or whenever you need its context. Use the desk session id from this session's context.")]
    async fn desk_work_item(&self, Parameters(p): Parameters<SessionParams>) -> Result<CallToolResult, McpError> {
        let sess = match self.client.get(&format!("/sessions/{}", p.session_id)).await {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let Some(item) = sess["work_item"]["id"].as_str() else {
            let ask = "This session is not on a work item yet. If the user's opening prompt or a handoff holds an issue key of a connected team (linear_teams lists them; a lookalike in prose is not one), call desk_set_work_item with it and say which in one line. Only with no such key, ask the user which item it is and call desk_set_work_item.";
            return Ok(CallToolResult::success(vec![ContentBlock::text(with_desk_notes(ask, &sess))]));
        };
        match self.client.get(&format!("/items/{}/brief", crate::uri::encode(item))).await {
            Ok(v) => Ok(CallToolResult::success(vec![ContentBlock::text(with_desk_notes(v["text"].as_str().unwrap_or_default(), &sess))])),
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
    #[tool(description = "Read a Linear issue: title, state, team, project, assignee, labels, description and its newest 25 comments, oldest first, each with its id and the id of the comment it replies under. The answer carries comments_total and, when some are not returned, comments_truncated and a comments_note; read older ones by calling again with skip. Works across every connected Linear workspace.")]
    async fn linear_get_issue(&self, Parameters(p): Parameters<IssueParams>) -> Result<CallToolResult, McpError> {
        let key = crate::uri::encode(p.key.trim());
        match self.client.get(&format!("/linear/issue/{key}?skip={}", p.skip.unwrap_or(0))).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "The user's open Linear issues (assigned to them, not done or canceled), in this session's desk's Linear project or team (its folder's desk, else its work item's), or every connected workspace when it is on no desk; most recently updated first.")]
    async fn linear_my_issues(&self) -> Result<CallToolResult, McpError> {
        match self.client.get("/linear/mine").await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Every connected Linear workspace's teams: each team's key, name and id, with the workspace's account label, never a token. A team with no project is listed too. Check a key here when Linear says no connected workspace has it: a key whose team is missing is wrong, or its workspace is not connected.")]
    async fn linear_teams(&self) -> Result<CallToolResult, McpError> {
        match self.client.get("/linear/teams").await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Find Linear issues by words in their title, within this session's desk's Linear project or team (everywhere when it is on no desk), or by key anywhere. The answer's scope names the project or team a word query searched; a key reads the issue from any connected workspace.")]
    async fn linear_search(&self, Parameters(p): Parameters<QueryParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/linear/search", json!({ "query": p.query })).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "List a team's Linear issues that are in one workflow state, by the state's name (\"Fleet Ready\"), optionally only those of one project (by name); most recently updated first. Not limited to this session's desk: any connected workspace's team, by its key. A state the team does not have is an error that lists the ones it has. The answer carries truncated, and a note when the 500-issue cap cut the list.")]
    async fn linear_list_issues(&self, Parameters(p): Parameters<StateListParams>) -> Result<CallToolResult, McpError> {
        match self.client.post("/linear/issues", json!({ "team": p.team_key, "state": p.state, "project": p.project })).await {
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

    #[tool(description = "Propose a comment on a Linear issue, or a thread of them (replies), or a reply in an existing thread (reply_to). On a work desk nothing is posted: the draft waits on the user's Desk tab, where they edit it if they want and press Send, so tell the user it is waiting there. A desk set to write directly posts it at once and the reply says so.")]
    async fn linear_draft_comment(&self, Parameters(p): Parameters<CommentDraftParams>) -> Result<CallToolResult, McpError> {
        let body = json!({ "kind": "linear_comment", "issue_key": p.issue_key, "body": p.body, "reply_to": p.reply_to,
                           "replies": p.replies, "session_id": p.session_id });
        match self.client.post("/drafts", body).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Propose a change to a Linear issue's title, description or both; a description given replaces the whole current one, so read the issue first and send it complete. On a work desk nothing changes until the user's Send on the Desk tab (a desk set to write directly applies it at once and the reply says so). The Send is refused if someone changed the description after the draft was made.")]
    async fn linear_draft_edit(&self, Parameters(p): Parameters<EditDraftParams>) -> Result<CallToolResult, McpError> {
        let body = json!({ "kind": "linear_edit", "issue_key": p.issue_key, "title": p.title,
                           "body": p.description.unwrap_or_default(), "session_id": p.session_id });
        match self.client.post("/drafts", body).await {
            Ok(v) => ok(v),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Propose a new Linear issue. On a work desk nothing is created: the draft waits on the user's Desk tab for their Send (a desk set to write directly creates it at once and the reply says so). If this session's work item has no issue yet, the new issue is attached to it once sent.")]
    async fn linear_draft_issue(&self, Parameters(p): Parameters<IssueDraftParams>) -> Result<CallToolResult, McpError> {
        let body = json!({ "kind": "linear_issue", "team_key": p.team_key, "title": p.title, "body": p.description, "session_id": p.session_id, "project": p.project });
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

    #[tool(description = "The desks — one per project: its Linear project, Harvest project and task, GitHub repos, firmware repos and saved bench — the one this session works on (`session`: its folder's desk, else its work item's, null for none), and the one the UI shows (`open`).")]
    async fn desk_list_desks(&self) -> Result<CallToolResult, McpError> {
        match self.client.get("/desks").await {
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
             item. Read Linear with linear_get_issue, linear_my_issues, linear_search and linear_list_issues, and list the \
             connected workspaces' team keys with linear_teams; \
             move a status with linear_set_status; propose a comment, an issue or a change to one with linear_draft_comment, \
             linear_draft_issue or linear_draft_edit — they wait for the user's Send on the Desk tab. Read Harvest with \
             harvest_entries and harvest_projects; time is never logged from a session: add notes with \
             desk_add_line and the user sends the tile. A rule the user sets for every session in this desk's \
             repos (always, never, from now on, in this repo) is a desk note, added with desk_add_desk_note, \
             never a line. desk_timer_start/stop track work away from Claude. \
             The session's context carries its desk session id; when the desk asks which work item a session \
             is on, first take an issue key of a connected team from the opening prompt or a handoff (a lookalike \
             in prose is not one), call desk_set_work_item with it and say which in one line; ask the user only \
             when there is no such key.",
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
    fn the_pull_is_a_session_tool_a_note_takes_a_kind_and_a_desk_note_its_session_and_text() {
        let tools = DeskMcp::tool_router().list_all();
        let tool = |name: &str| tools.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no tool {name}"));
        let pull = serde_json::to_value(&tool("desk_work_item").input_schema).unwrap();
        assert_eq!(pull["required"], json!(["session_id"]));
        let line = serde_json::to_value(&tool("desk_add_line").input_schema).unwrap();
        assert!(line["properties"]["kind"].is_object(), "{line}");
        assert!(line["properties"]["item"].is_object(), "{line}");
        assert_eq!(line["required"], json!(["session_id", "text"]));
        let add = serde_json::to_value(&tool("desk_add_repo").input_schema).unwrap();
        assert_eq!(add["required"], json!(["session_id", "repo"]));
        let desk_note = tool("desk_add_desk_note");
        assert_eq!(serde_json::to_value(&desk_note.input_schema).unwrap()["required"], json!(["session_id", "text"]));
        assert!(desk_note.description.as_deref().unwrap_or_default().contains("Never part of Harvest"));
    }

    #[test]
    fn the_team_list_is_a_linear_tool_that_takes_nothing() {
        let tools = DeskMcp::linear_router().list_all();
        let teams = tools.iter().find(|t| t.name == "linear_teams").expect("no tool linear_teams");
        let schema = serde_json::to_value(&teams.input_schema).unwrap();
        assert!(schema["required"].as_array().is_none_or(|r| r.is_empty()), "{schema}");
        assert!(teams.description.as_deref().unwrap_or_default().contains("never a token"));
    }

    #[test]
    fn the_pull_ends_with_the_desk_notes_the_session_was_given_and_the_brief_carries_none_itself() {
        use crate::store::testing::UtcDays;
        let mut st = crate::store::State::default();
        let k = st.put_desk(crate::store::Desk { name: "Alpha".into(), ..Default::default() }, 0);
        let w = st.create_item("Loader", None, 0);
        st.items.get_mut(&w).unwrap().desk_id = Some(k.clone());
        st.add_desk_note(&k, "in repo X, only app Y on board Z matters", "user", None, 0).unwrap();
        st.add_desk_note(&k, "file F is another engineer's:\nreport findings, never open a fix branch", "agent", Some("s"), 0).unwrap();
        let brief = crate::brief::render(&st, &crate::server::Cache::default(), &w, None, None, &UtcDays).unwrap();
        assert!(!brief.contains("board Z") && !brief.contains("Desk notes"), "{brief}");
        // What GET /sessions/{sid} answers.
        let sess = json!({ "desk_id": k, "desk_notes": st.desk_notes_of(&k) });
        let pull = with_desk_notes(&brief, &sess);
        assert!(pull.starts_with(brief.trim_end()), "{pull}");
        assert!(pull.ends_with(
            "\n\n## Desk notes\nStanding notes for every session in this desk's repos, the ones this session was given at its start; \
             follow them.\n- in repo X, only app Y on board Z matters\n\
             - file F is another engineer's:\n  report findings, never open a fix branch (added by an agent)\n"
        ), "{pull}");
        assert_eq!(with_desk_notes(&brief, &json!({ "desk_id": null, "desk_notes": [] })), brief);
    }
}
