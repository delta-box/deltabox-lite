<div align="center">

# Deltabox-lite

**Lightweight, agent-native sandboxing with diff · commit · abort.**

Give agents a confined workspace with explicit diff, commit, and abort semantics.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.87+-orange.svg)](#)
[![Platform](https://img.shields.io/badge/platform-linux%20%7C%20macOS-lightgrey.svg)](#)
[![API](https://img.shields.io/badge/API-JSON--RPC-blueviolet.svg)](docs/protocol.md)
[![Pi](https://img.shields.io/badge/pi-deltabox-purple.svg)](https://github.com/Ddnirvana/pi-deltabox)

</div>

deltabox-lite is a standalone sandbox runtime for AI coding agents. Agents keep
full execution freedom inside a confined filesystem view, and every change
they make can be **reviewed**, **committed**, or **aborted** - per task or per
tool call. Linux defaults to a commit-biased FUSE view for low command latency;
the private-copy mode keeps changes hidden from the host until commit.

Think of it as Git for agent sessions: less ceremony, more control, and no
anxiety about binary or large files.

## Demo

The JSON-RPC protocol in action — sandboxed edits, diff, abort, redo, commit:

![deltabox-lite JSON-RPC demo](assets/deltabox-lite.gif)

## Highlights

- **Native isolation, not a container** - Linux composes Sandlock/Landlock/
  seccomp with DeltaFS FUSE; macOS uses Seatbelt plus APFS-cloned shadows.
- **Service-free durable sessions** - Linux starts one session-scoped
  filesystem helper automatically. Durable files remain authoritative across
  independent CLI processes; no global daemon is required.
- **Two filesystem contracts** - `cbcow-through` makes Linux changes visible
  immediately and makes commit an O(1) acceptance operation. `copy-private`
  keeps the host unchanged until commit and is the compatibility fallback.
- **Diff / commit / abort** - unified text diffs, binary and large-file
  markers, and a linear checkpoint chain. Abort restores the selected
  checkpoint; commit accepts the current state.
- **Review-scope savepoints** — task and tool adapters choose the correct abort
  target; normal commit and abort do not require a user-created checkpoint.
- **Agent-agnostic** — a line-delimited JSON-RPC protocol over stdio. The Pi
  adapter ships as [pi-deltabox](https://github.com/Ddnirvana/pi-deltabox);
  adapters for other agents speak the same protocol.
- **Policy-first** — filesystem read/write rules, secret-path denial, network
  allow/audit, deny-all and allowlist modes, scrubbed child environments, and
  bounded writes/storage.

## How it works

```mermaid
flowchart LR
    A[CLI or agent adapter] --> C[Session coordinator]
    C --> T[Linux cbcow-through]
    T --> H[Session-scoped DeltaFS helper]
    H --> P[(Host-visible LIVE tree)]
    C --> R[copy-private fallback]
    R --> S[(Private working tree)]
    S -->|commit| P
    O[Optional deltabox serve] -.-> C
```

On Linux with FUSE available, `session start` launches a helper that mounts the
host project through the commit-biased state engine. Writes update the host
tree through DeltaFS after recording rollback data; commit accepts the current
state and abort replays undo records. In `copy-private`, each command updates a
durable private working tree and commit publishes it to the project.
`deltabox serve` is only an optional JSON-RPC transport and is never the source
of session truth. See [session semantics](docs/session-semantics.md).

## Quick start

Build the runtime:

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

Start a durable session and use it from independent CLI processes:

```bash
deltabox session start /abs/path/my-project
# prints a session id
deltabox exec <id> --project /abs/path/my-project -- npm install
deltabox diff <id> --project /abs/path/my-project
deltabox commit <id> --project /abs/path/my-project
```

Linux requests `cbcow-through` by default and visibly falls back to
`copy-private` if FUSE is unavailable. Select the contract explicitly when a
workflow requires it:

```bash
deltabox session start /abs/path/my-project --fs-mode copy-private
deltabox session start /abs/path/my-project --fs-mode cbcow-through --require-fs-mode
```

No service needs to be started first. To use the optional JSON-RPC transport:

```bash
cd my-project
deltabox serve
```

The optional server speaks line-delimited JSON-RPC (see
[docs/protocol.md](docs/protocol.md)). A minimal session looks like:

```json
{"id":1,"method":"session.start","params":{"project":"/abs/path/my-project"}}
```

From there, `session.exec`, `fs.write`, `checkpoint.create`, `diff.list`,
`changes.commit`, `changes.abort`, and the `review.*` scope methods drive the
whole lifecycle.

The fastest way to see it with an agent is the Pi extension:

```bash
pi install npm:pi-deltabox
```

## Policy

`session start --policy policy.toml` accepts filesystem, network, and resource
limits. The default agent policy uses the `allow_audit` network mode, denies
common secret paths, passes only a small environment allowlist to commands,
caps one direct write at 16 MiB, and caps pending added/modified file content at
1 GiB. Network policy can instead deny all access or allow only explicit
host-and-port targets:

```toml
[network]
mode = "allowlist"
allow = ["registry.npmjs.org:443", "127.0.0.1:3000"]

[limits]
max_write_bytes = 16777216
max_storage_bytes = 1073741824
```

See [policy reference](docs/policy.md) for the full schema and enforcement
semantics, and [macOS compatibility](docs/macos.md) for path mapping and tool
support.

## Performance

Measured on x86-server (Linux 6.18, ext4, release build). Each row reports 50
no-op commands after five warmups. Deltabox uses independent CLI processes over
one `cbcow-through` session.

| Runner | 10 files p50 / p95 | 200 files p50 / p95 | 1000 files p50 / p95 |
|---|---:|---:|---:|
| `cbcow-through` | 13.7 / 15.5 ms | 14.3 / 15.4 ms | 13.2 / 14.7 ms |
| raw Sandlock | 8.1 / 9.4 ms | 8.2 / 9.0 ms | 8.1 / 8.9 ms |
| plain shell | 1.3 / 1.7 ms | 1.2 / 1.6 ms | 1.2 / 1.5 ms |

The no-op path does not scan or copy the project per command. Empty DeltaFS
checkpoint and commit operations remain below 1 ms in the scaling benchmark.

Measured on an Apple-silicon MacBook Air (macOS 14.6, APFS, release build).
Each row reports the p50 of 30 interleaved no-op commands after three warmups
over a pre-created project and session.

| Runner | 10 files | 200 files | 1000 files |
|---|---:|---:|---:|
| service-free CLI (`copy-private`) | 88 ms | 142 ms | 333 ms |
| optional `serve` | 84 ms | 138 ms | 324 ms |
| Anthropic sandbox-runtime 0.0.70 | 137 ms | 137 ms | 137 ms |
| Carderne sandbox-runtime 0.0.69 | 132 ms | 133 ms | 129 ms |
| raw `sandbox-exec` | 10 ms | 10 ms | 10 ms |
| plain shell | 3 ms | 3 ms | 3 ms |

The raw Seatbelt row denies network but allows filesystem access, so it is a
launch-overhead baseline rather than a sandbox with Deltabox semantics.
Deltabox clones and validates a crash-recoverable candidate tree for every
arbitrary command. APFS makes file contents copy-on-write, but directory entry
creation and metadata validation still scale with the number of files.

## Project layout

- `crates/deltabox-core` — sessions, checkpoints, diff/commit/abort, policy,
  journal, platform backends.
- `crates/deltabox-cli` — the `deltabox` binary (CLI + stdio JSON-RPC server).
- `deltafs-fuse` — the standalone commit-biased state engine, FUSE adapter,
  helper, and local control protocol.
- `examples/rpc-demo.mjs` — a runnable JSON-RPC demo.
- `docs/protocol.md` — the stable JSON-RPC contract.
- `docs/policy.md` — policy schema and enforcement boundaries.
- `docs/macos.md` — macOS backend behavior and compatibility matrix.


## Citation

If you find deltabox-lite useful, please cite the original paper of DeltaBox (which is based on microVM) or this github repo:

```bibtex
@misc{dong2026deltaboxscalingstatefulai,
      title={DeltaBox: Scaling Stateful AI Agents with Millisecond-Level Sandbox Checkpoint/Rollback}, 
      author={Yunpeng Dong and Jingkai He and Shiqi Liu and Yuze Hou and Dong Du and Zhonghu Xu and Si Yu and Baochuan Yang and Yubin Xia and Haibo Chen},
      year={2026},
      eprint={2605.22781},
      archivePrefix={arXiv},
      primaryClass={cs.OS},
      url={https://arxiv.org/abs/2605.22781}, 
}
```


## License

Apache-2.0.

## Acknowledgments

This project utilizes excellent work of existing sandbox projects like **[`sandlock`](https://github.com/multikernel/sandlock)**.


**⭐ Star this repo if you find deltabox-lite helpful!**
