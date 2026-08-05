# deltabox-lite

Lightweight, agent-native sandbox with **diff / commit / abort**.

deltabox-lite gives coding agents (Pi, Codex, Claude Code) high execution freedom
while keeping every system change (e.g., files) reviewable and discardable — like Git, but
built for agent sessions and without Git's coarse handling of binary and large
files.

This repository is the **core sandbox runtime**: an agent-agnostic CLI + JSON-RPC
server. The Pi extension lives in a separate repo (`pi-deltabox`).


## Build

```bash
cargo build
cargo test
```

## Layout

- `crates/deltabox-core` — library: sessions, checkpoints, diff/commit/abort,
  policy, journal, platform backends.
- `crates/deltabox-cli` — the `deltabox` binary (CLI + stdio JSON-RPC server).
- `docs/protocol.md` — the JSON-RPC protocol contract.

## License

Apache-2.0.

