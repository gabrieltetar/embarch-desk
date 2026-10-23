---
name: desk
description: How a session works with the EmbArch desk — the one access point to Linear and Harvest — answering "which work item is this?", pulling everything about the work item, reading and moving Linear issues, proposing comments and issues as drafts, taking notes on the work item, tracking time away from Claude, and what never to do (log or approve time). Use whenever a task touches Linear, Harvest or time tracking, or the session context mentions the EmbArch Desk or a desk session id.
---

# Working with the EmbArch desk

The desk is **the one access point to Linear and Harvest**, and it tracks this session's time on a **work item** — usually one Linear issue — shown to the user as a day tile they send to Harvest (✓) or discard (✕).

## Desks

One desk per project — a Linear project (or a whole team), a Harvest project and task, its GitHub repos, firmware repo and saved bench. **One desk is open suite-wide**, and your Linear and GitHub tools are scoped to it; with no desk open they search everything. `desk_list_desks` says which exist and which is open.

- **Switch only when the user says so**, with `desk_open` ("none" closes it). If the context says this session started in another desk's repo, ask the user once whether to switch; either way the session's time goes to its own desk.
- **A new standalone work item needs its desk** — ask which (`desk` on `desk_set_work_item`, `desk_create_item`, `desk_timer_start`).
- `embarch-api` tools build the open desk's firmware repo when no project is named.

## Linear

- Read with `linear_get_issue` (description and latest comments), `linear_my_issues`, `linear_search`.
- Move a status with `linear_set_status` — it goes straight through.
- **Write prose only as a draft**: `linear_draft_comment`, `linear_draft_issue`. Nothing is posted; the draft waits on the user's Desk tab for their Send. Write it the way it should be posted, then tell the user it is waiting there. If a voice or review skill applies to text going out under the user's name, follow it before drafting.

## Notes on the work item

The work item is where the work's knowledge gathers. **`desk_work_item` returns everything the desk holds about it** — the issue with its description and latest comments, PRs, sessions, notes (each memory file's newest state, the latest lines) and bench activity, newest first. Call it when starting on the item and whenever you need its context, rather than reassembling it from Linear and the repo.

**Take notes unasked, with `desk_add_line`** — too many rather than too few; the user cuts. One short concrete sentence each, with a `kind`:

- `done` (the default) — a piece of work finished without a commit: "Flashed the loader; the board boots to the shell."
- `finding` — something learned: "The flash failures came from a stale build directory."
- `decision` — a choice made, and why: "Kept the loader in RRAM; SRAM is short by 12 KB."
- `handoff` — what the next session needs to know: "The PR waits on the reviewer's question about the timeout."
- `todo` — something left to do: "The retry path still lacks a test."

Commits and Claude Code memory files are added automatically — a write to `~/.claude/projects/*/memory/` becomes the item's memory note, its versions kept — so do not repeat them. **What goes to Harvest is the summary of that day's notes.**

## Harvest and time

- **Never log time from a session.** There is no tool for it, by design: the session's time is already captured on its tile.
- Read what is logged with `harvest_entries` (default this week) and `harvest_projects`.
- "Start tracking X" / "stop tracking" for work away from Claude — the bench, a call, a meeting — is `desk_timer_start` / `desk_timer_stop`.

## The work item

1. **When the context says the session has no work item, ask the user in one line** which one it is, offering the open cards the desk listed, a Linear issue key, or a title for a new standalone item. Then call `desk_set_work_item` with the desk session id from the context. If the user says this is not tracked work, call it with `untracked: true`. Keep doing the user's actual request in the same turn; the question is one line, not a gate.
2. **If the user moves to different work**, call `desk_set_work_item` again with the new item.

**Never approve, discard or archive anything, and never send a draft.** Those are the user's, on the Desk tab.
