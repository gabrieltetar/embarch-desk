---
name: desk
description: How a session works with the EmbArch desk — the one access point to Linear and Harvest — answering "which work item is this?", reading and moving Linear issues, proposing comments and issues as drafts, adding a milestone line to the time tile, tracking time away from Claude, and what never to do (log or approve time). Use whenever a task touches Linear, Harvest or time tracking, or the session context mentions the EmbArch Desk or a desk session id.
---

# Working with the EmbArch desk

The desk is **the one access point to Linear and Harvest**, and it tracks this session's time on a **work item** — usually one Linear issue — shown to the user as a day tile they send to Harvest (✓) or discard (✕).

## Linear

- Read with `linear_get_issue` (description and latest comments), `linear_my_issues`, `linear_search`.
- Move a status with `linear_set_status` — it goes straight through.
- **Write prose only as a draft**: `linear_draft_comment`, `linear_draft_issue`. Nothing is posted; the draft waits on the user's Desk tab for their Send. Write it the way it should be posted, then tell the user it is waiting there. If a voice or review skill applies to text going out under the user's name, follow it before drafting.

## Harvest and time

- **Never log time from a session.** There is no tool for it, by design: the session's time is already captured on its tile.
- Read what is logged with `harvest_entries` (default this week) and `harvest_projects`.
- **Add a line with `desk_add_line` when a piece of work ends without a commit** — an investigation's finding, a flash or study result, a decision reached. One short past-tense sentence, concrete: "Found the flash failures came from a stale build directory." Commits are added automatically; do not repeat them.
- "Start tracking X" / "stop tracking" for work away from Claude — the bench, a call, a meeting — is `desk_timer_start` / `desk_timer_stop`.

## The work item

1. **When the context says the session has no work item, ask the user in one line** which one it is, offering the open cards the desk listed, a Linear issue key, or a title for a new standalone item. Then call `desk_set_work_item` with the desk session id from the context. If the user says this is not tracked work, call it with `untracked: true`. Keep doing the user's actual request in the same turn; the question is one line, not a gate.
2. **If the user moves to different work**, call `desk_set_work_item` again with the new item.

**Never approve, discard or archive anything, and never send a draft.** Those are the user's, on the Desk tab.
