---
name: desk
description: How a session works with the EmbArch desk — answering "which work item is this?", adding a milestone line to the time tile, and what never to do (approve time). Use when the session context mentions the EmbArch Desk or a desk session id.
---

# Working with the EmbArch desk

The desk tracks this session's time on a **work item** — usually one Linear issue — and shows it to the user as a tile they approve (✓) into Harvest or discard (✕). Your part is small:

1. **When the context says the session has no work item, ask the user in one line** which one it is, offering the open cards the desk listed, a Linear issue key, or a title for a new standalone item. Then call `desk_set_work_item` with the desk session id from the context. If the user says this is not tracked work, call it with `untracked: true`. Keep doing the user's actual request in the same turn; the question is one line, not a gate.
2. **Add a line with `desk_add_line` when a piece of work ends without a commit** — an investigation's finding, a flash or study result, a decision reached. One short past-tense sentence, concrete: "Found the flash failures came from a stale build directory." Commits are added automatically; do not repeat them.
3. **If the user moves to different work**, call `desk_set_work_item` again with the new item.

**Never approve, discard or archive anything.** Those are the user's, on the Desk tab, and no tool does them.
