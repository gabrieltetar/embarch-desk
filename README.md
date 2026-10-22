# embarch-desk

The service behind EmbArch's **Desk** tab: work items that join a Linear issue to its GitHub PRs, the Claude Code sessions that worked on it, its bench activity and its time — and day tiles you send to Harvest with a ✓.

One binary, three roles: `embarch-desk serve` (a systemd user service on `127.0.0.1:4891`), `embarch-desk hook` (what the Claude Code plugin's hooks run), `embarch-desk mcp` (the agent-facing tools). The UI is [embarch-ui](https://github.com/gabrieltetar/embarch-ui)'s Desk tab.

## Install

```sh
cargo build --release
install -m 755 target/release/embarch-desk ~/.local/share/embarch/bin/
embarch-desk service install                 # systemd user unit, starts now and at login
embarch-desk ignore add ~/path/to/untracked  # repos whose sessions are never tracked
claude plugin marketplace add "$PWD"
claude plugin install embarch-desk@embarch
```

## Connect

On the Desk tab, **Connectors**, or from a shell with the token on stdin:

```sh
embarch-desk connect linear <workspace>   < key    # Linear: Settings → Security & access → Personal API keys
embarch-desk connect harvest <name>       < token  # id.getharvest.com/developers → personal access token
embarch-desk connect slack <workspace>    < token  # an app from slack-manifest.json; its User OAuth Token
```

GitHub needs nothing if `gh` is logged in.

Design: [embarch-doc/embarch-desk](https://github.com/gabrieltetar/embarch-doc/tree/main/embarch-desk). MIT.
