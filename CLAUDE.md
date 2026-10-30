# embarch-desk

## Docs

**Four files, not one.** Current truth: [spec.md](../embarch-doc/embarch-desk/spec.md). Why it is that way: [decisions.md](../embarch-doc/embarch-desk/decisions.md) — a decision number addresses this sub-project, not a file. Unresolved: [open.md](../embarch-doc/embarch-desk/open.md).

Update them proactively per [../embarch-doc/DOC-PROTOCOL.md](../embarch-doc/DOC-PROTOCOL.md) whenever a notable design decision, feature, or status change happens here — §4 says when, §5 says how, and history goes in a `changelog.d/` fragment rather than into a doc.

**Connector data never lands here.** Issue titles, PR titles, client names and hours live in the user's own `~/.local/share/embarch-desk/`; tokens in `~/.config/embarch-desk/`. Tests use synthetic values only.

## Git

**Work directly on `main` — no feature branches, no PRs (2026-08-25).** Overrides the general "branch before committing to the default branch" default, for this suite only; ends when the repo owner explicitly says so. See [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §6.

## Gate

`cargo test` and `cargo clippy --all-targets -- -D warnings`. The Desk tab's browser drive lives in embarch-ui: `tests/browser/drive_desk.py`.
