---
name: desk
description: How a session works with the EmbArch desk — the one access point to Linear and Harvest — answering "which work item is this?", pulling everything about the work item, reading and moving Linear issues, proposing comments and issues as drafts, taking notes on the work item, keeping the desk's standing notes, tracking time away from Claude, and what never to do (log or approve time). Use whenever a task touches Linear, Harvest or time tracking, or the session context mentions the EmbArch Desk or a desk session id.
---

# Working with the EmbArch desk

The desk is **the one access point to Linear and Harvest**, and it tracks this session's time on a **work item** — usually one Linear issue — shown to the user as a day tile they send to Harvest (✓) or discard (✕).

## Desks

One desk per project — a Linear project (or a whole team), a Harvest project and task, its GitHub repos, firmware repo and saved bench. **This session works on its own desk**: the desk holding its folder, else its work item's desk. Your Linear tools are scoped to it; on no desk they search everything. `desk_list_desks` says which desks exist, which one this session works on (`session`) and which one the UI shows (`open`).

- **Nothing to switch.** Other sessions work on their own desks at the same time, and the desk the user picks in the UI only chooses the cards it shows.
- **A new standalone work item needs its desk** — ask which (`desk` on `desk_set_work_item`, `desk_create_item`, `desk_timer_start`).
- `embarch-api` tools build this session's desk's firmware repo when no project is named.

## Linear

- Read with `linear_get_issue` (description and the newest 25 comments, each with its `id` and the `parent` it replies under; `comments_total` and `comments_note` say when some were not returned, and `skip` reads older ones), `linear_my_issues`, `linear_search`, and `linear_list_issues` for a team's queue (every issue of a team key in a named state such as "Fleet Ready", optionally one project; `truncated` and a `note` say when its 500-issue cap cut the list).
- An issue's screenshots and files are not in its text: `linear_get_issue` lists them under `files` (every upload link in the description and comments, and the attachment records, each with where it was found). Read one with `linear_get_file` (the issue key and the link as listed; any other link is refused). An image comes back as an image, a text file as text, anything else as a path under the desk's data folder. Do it whenever an issue's text points at a picture or file, such as "see the screenshot"; the links expire within minutes, so use a fresh read's.
- **Do not fall back to Claude's own Linear connector** for a read or write the desk has no tool for. Say the desk lacks it and file the gap against the desk (embarch-desk decision 29 applies; there is no second route).
- `linear_teams` lists each connected workspace's teams (key, name, id) under its account label. When a key is refused with "no connected Linear workspace has", check its team key there before asking the user.
- Move a status with `linear_set_status` — it goes straight through.
- **Write prose only as a draft**: `linear_draft_comment`, `linear_draft_issue`, `linear_draft_edit`. Nothing is posted; the draft waits on the user's Desk tab for their Send. Write it the way it should be posted, then tell the user it is waiting there. If a voice or review skill applies to text going out under the user's name, follow it before drafting.
- **An edit replaces the whole description**: read the issue first and send it complete. Its Send is refused if the description changed after you drafted; draft again from the current text.
- **A long comment is a thread**: the point in `body`, detail in `replies`, posted under it by the same Send. Answer someone's comment with `reply_to` set to its `id`.
- **An issue in the EmbArch workspace (team `EMB`)** takes the shape `embarch-doc/LINEAR-PROTOCOL.md` gives it, and goes through `embarch-doc/scripts/fleet-linear.py`, not these tools.

## Notes on the work item

The work item is where the work's knowledge gathers. **`desk_work_item` returns everything the desk holds about it** — the issue with its description and latest comments, PRs, sessions, notes (each memory file's newest state, the latest lines) and bench activity, newest first. Call it when starting on the item and whenever you need its context, rather than reassembling it from Linear and the repo.

**Take notes unasked, with `desk_add_line`** — too many rather than too few; the user cuts. One short concrete sentence each, with a `kind`:

- `done` (the default) — a piece of work finished without a commit: "Flashed the loader; the board boots to the shell."
- `finding` — something learned: "The flash failures came from a stale build directory."
- `decision` — a choice made, and why: "Kept the loader in RRAM; SRAM is short by 12 KB."
- `handoff` — what the next session needs to know: "The PR waits on the reviewer's question about the timeout."
- `todo` — something left to do: "The retry path still lacks a test."

**A note about a different work item goes to that item.** When you learn something about another issue while working on this one, call `desk_add_line` with `item` set to its issue key or desk item id, so it lands on that item's day and not on this one. Leave `item` out for your own. An unknown `item` is an error and nothing is saved: check the key.

Commits and Claude Code memory files are added automatically — a write to `~/.claude/projects/*/memory/` becomes the item's memory note, its versions kept — so do not repeat them. **What goes to Harvest is the summary of that day's notes.**

## Desk notes

A desk can hold **standing notes**: working agreements for every session in its repos, such as "in repo X, the only build that matters is app Y on board Z" or "file F is another engineer's; report findings, never open a fix branch". The context lists them when the session starts, an agent's marked as such, and `desk_work_item` repeats the ones this session was given. **Follow them; if one conflicts with what the user asks now, ask.** Do not copy them into memory files or work-item notes.

- **When the user sets such a rule** (always, never, from now on, in this repo), or agrees when you ask whether something should hold for every session here, add it with `desk_add_desk_note`, which puts it on the desk this session was given. One rule per note, at most 500 characters; a desk holds 4,000.
- **Never progress, findings or task state**: those are `desk_add_line`. A desk note never reaches Harvest.
- There is no tool to edit or delete one: the user does that on the Desk tab.

## Harvest and time

- **Never log time from a session.** There is no tool for it, by design: the session's time is already captured on its tile.
- Read what is logged with `harvest_entries` (default this week) and `harvest_projects`.
- "Start tracking X" / "stop tracking" for work away from Claude — the bench, a call, a meeting — is `desk_timer_start` / `desk_timer_stop`.

## The work item

1. **When the context says the session has no work item, look in the opening prompt and any handoff first.** An issue key of a connected team there (`linear_teams` lists the keys; a lookalike in prose is not one) is set with `desk_set_work_item` without asking, and you say which item you chose in one line. **Only with no such key, ask the user in one line** which one it is, offering the open cards the desk listed, a Linear issue key, or a title for a new standalone item. Then call `desk_set_work_item` with the desk session id from the context. If the user says this is not tracked work, call it with `untracked: true`. Keep doing the user's actual request in the same turn; the question is one line, not a gate.
2. **If the user moves to different work**, call `desk_set_work_item` again with the new item.

**Never approve, discard or archive anything, and never send a draft.** Those are the user's, on the Desk tab.
