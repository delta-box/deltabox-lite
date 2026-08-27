# Provider architecture

Sessions are composed from two orthogonal providers:

- **Isolation provider** – confines one `session.exec` command.
- **Storage provider** – keeps the rollback snapshots that make
  `diff` / `commit` / `abort` work within its stated coverage.

Today the production providers are DeltaFS FUSE + Sandlock (Linux CB-CoW),
APFS/FSEvents + Seatbelt (macOS native), the service-free copy-private engine,
and the zero-FUSE GitGhost engine with Bubblewrap or native-only Docker
isolation.

## GitGhost storage (`--fs-mode git`)

Git-backed ghost sessions run the agent directly against the real project
tree inside a Bubblewrap sandbox or ephemeral Docker container and keep every
snapshot as a commit in a private bare side repository.

### Data flow

```text
deltabox CLI / serve
  └─ SessionCoordinator
       └─ GitSessionManager (fs_mode=git, routed before any FUSE probing)
            ├─ BubblewrapIsolation  → bwrap --bind <project>:/workspace ...
            ├─ DockerIsolation      → docker run <project>:/workspace ubuntu:22.04 ...
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
  the pending refs and establish the accepted baseline. Abort promotes its
  restored ghost ref directly instead of rescanning the resulting workspace;
  commit snapshots the newly accepted workspace. Objects stay in the side repo
  until the session closes.
- `diff` maps `git diff-tree --name-status/--numstat` plus blob sizes onto
  the protocol `Change` shape, with per-file `-U3` text patches (8-path
  chunks, 10 MiB per-file budget, `core.quotepath=false`). Binary detection
  is numstat `-`, size ≥ 10 MiB, or a NUL byte in the first 8 KiB.
- `abort` rewinds with `read-tree` + `checkout-index -a -f`, deletes
  untracked files (`ls-files --others --exclude-standard`), and drops the
  undo refs. Restore is byte-for-byte for files present in the ghost index.

### Capability boundaries

- **Zero FUSE**: no DeltaFS mount or helper process, `/dev/fuse`, or
  `fusermount`. Commands use either `bwrap` or the Docker CLI; snapshots use
  host Git. `SessionInfo.mountpoint`/`helper_pid` are absent.
- **Ignored files are outside the contract**: files matched by the project's
  `.gitignore` never enter snapshots, so abort leaves them untouched.
- **Policy scope differs from Deltabox isolation**: `fs.read` and `fs.write`
  enforce policy deny paths and write limits. Commands inside Bubblewrap can
  access the whole project bind mount; v1 does not translate policy deny paths
  into submounts. Bubblewrap still denies network, scrubs the environment, and
  keeps host system paths read-only.
- **No warm worker**: every `exec` is a fresh `bwrap` sandbox with
  `--unshare-net` (deny-all), read-only `/usr /lib /lib64 /bin /etc`,
  fresh `/tmp`, and `--die-with-parent`. Environment is scrubbed to an
  allowlist plus `HOME`/`TMPDIR`; user env overrides are applied last.
- **Native-only Docker option**: `git+docker` uses the fixed, preinstalled
  `ubuntu:22.04` image. Each command gets a read-only root, writable project
  bind at `/workspace`, private `/tmp`, current host UID/GID, no network, no
  capabilities, and `no-new-privileges`. The CLI and stdio protocol do not
  enable this option, and Deltabox never pulls the image automatically.
- **Review scopes use ghost savepoints**: `review.begin` pins the current
  workspace in the side repository, `review.diff` compares against that ref,
  Review abort restores and promotes that savepoint as the session baseline.
  The active scope is persisted beside the session record, so independent CLI
  processes and `serve` restarts preserve it. Strict durability remains
  unsupported in git mode.
- **Session operations share the CB-CoW lock primitive**: standalone commands
  and `serve` serialize same-session work with a persistent file plus kernel
  `flock`. Contending operations wait, and process exit releases the lock
  without PID files or per-operation lock-file `fsync`.
- **Linux-only**: `bubblewrap` must be installed and able to create user
  namespaces; otherwise `session.start` fails (no fallback, no downgrade).
- **Server API compatibility**: no new methods, no protocol version change.
  `session.start` gains one optional `isolation` field (`serde(default)`);
  `fs_mode` gains the `git` value; every other parameter and every response
  structure is unchanged.

## Isolation capability matrix

| Isolation | Command environment | Network | Project command access | Native file access |
| --- | --- | --- | --- | --- |
| `deltabox` | Platform worker profile plus explicit request environment | Policy-controlled | Policy-controlled | N-API bytes |
| `bubblewrap` | Host allowlist plus explicit overrides; fresh `HOME`/`TMPDIR` | Deny-all | Entire project bind mount | N-API bytes with policy checks |
| `docker` | Image environment plus host allowlist and explicit overrides; fresh `HOME`/`TMPDIR` | Deny-all | Entire project bind mount | N-API bytes with policy checks |
| `none` | Inherits the embedding Node process environment | Unrestricted | Direct mount | Direct mount plus exact-path recording |

`isolation` is the stable logical profile. `SessionInfo.isolation_provider`
reports the implementation selected on the host: `sandlock`, `seatbelt`,
`bubblewrap`, or `docker`; it is absent for `none`.
