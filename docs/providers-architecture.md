# Provider architecture

Sessions are composed from two orthogonal providers:

- **Isolation provider** – confines one `session.exec` command.
- **Storage provider** – keeps the rollback snapshots that make
  `diff` / `commit` / `abort` work.

Today the production providers are DeltaFS FUSE + Sandlock (Linux CB-CoW),
APFS/FSEvents + Seatbelt (macOS native), the service-free copy-private engine,
and the zero-FUSE **bubblewrap + GitGhost** engine.

## GitGhost storage (`--fs-mode git`)

Git-backed ghost sessions run the agent directly against the real project
tree inside a bubblewrap sandbox and keep every snapshot as a commit in a
private bare side repository.

### Data flow

```text
deltabox CLI / serve
  └─ SessionCoordinator
       └─ GitSessionManager (fs_mode=git, routed before any FUSE probing)
            ├─ BubblewrapIsolation  → bwrap --bind <project>:/workspace ...
            └─ GitGhostStorage      → git --git-dir=undo.git --work-tree=<project>
```

- The project root never needs to be a git repository. If it is one, its
  `.git`, index, HEAD, and branches are never read or written: every git
  command runs with `--git-dir` on the side repo, `--work-tree` on the
  project, and a per-operation temporary `GIT_INDEX_FILE`.
- The side repo (`undo.git`) lives under the external state base
  (`DELTABOX_STATE_DIR`, else `XDG_STATE_HOME`/`~/.local/state`), never
  inside the project, so session state is never bind-mounted into the
  sandbox and snapshots never contain it.
- Snapshot `N` is a commit pinned at `refs/deltabox/undo/<N>` whose parent is
  the previous ghost commit. The session's baseline is its first snapshot;
  `checkpoint.create` pins explicit savepoints; `commit` and `abort` retire
  the pending refs and re-baseline from the resulting workspace. Objects stay
  in the side repo until the session closes.
- `diff` maps `git diff-tree --name-status/--numstat` plus blob sizes onto
  the protocol `Change` shape, with per-file `-U3` text patches (8-path
  chunks, 10 MiB per-file budget, `core.quotepath=false`). Binary detection
  is numstat `-`, size ≥ 10 MiB, or a NUL byte in the first 8 KiB.
- `abort` rewinds with `read-tree` + `checkout-index -a -f`, deletes
  untracked files (`ls-files --others --exclude-standard`), and drops the
  undo refs. Byte-for-byte restore is guaranteed for everything git tracks.

### Capability boundaries

- **Zero FUSE**: no mount, no filesystem/command helper process, no
  `/dev/fuse`, no `fusermount`. The only child processes are `bwrap` and
  `git`. `SessionInfo.mountpoint`/`helper_pid` are absent.
- **Ignored files are outside the contract**: files matched by the project's
  `.gitignore` never enter snapshots, so abort leaves them untouched.
- **No warm worker**: every `exec` is a fresh `bwrap` sandbox with
  `--unshare-net` (deny-all), read-only `/usr /lib /lib64 /bin /etc`,
  fresh `/tmp`, and `--die-with-parent`. Environment is scrubbed to an
  allowlist plus `HOME`/`TMPDIR`; user env overrides are applied last.
- **`review.*` and strict durability are unsupported**: git mode returns
  explicit unsupported errors at runtime; the methods and result shapes are
  unchanged at the protocol level.
- **Linux-only**: `bubblewrap` must be installed and able to create user
  namespaces; otherwise `session.start` fails (no fallback, no downgrade).
- **Server API compatibility**: no new methods, no protocol version change.
  `session.start` gains one optional `isolation` field (`serde(default)`);
  `fs_mode` gains the `git` value; every other parameter and every response
  structure is unchanged.
