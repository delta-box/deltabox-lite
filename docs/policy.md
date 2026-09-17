# Policy reference

Pass a TOML policy to `deltabox session start <project> --policy <file>`. The
policy path is persisted in session metadata and reloaded for every independent
CLI or optional `serve` operation.

## Filesystem

```toml
[filesystem]
read = [".", "/usr", "/bin", "/lib", "/lib64", "/etc", "/run"]
write = ["."]
deny = [".env", ".env.*", "*.pem", "*.key", ".ssh", ".aws"]
```

Relative read/write paths are resolved from the durable working project.
Existing deny matches become sandbox deny rules. A command that creates,
modifies, or deletes a deny-matching path is rejected before its COW branch can
be materialized, so the durable working view remains unchanged.

Supervisor-side reads canonicalize the final target. A symlink that escapes the
working tree, is absolute, or resolves to a denied path is rejected. Diff never
dereferences such a link and therefore cannot expose its target bytes.

`/dev/null` is writable. On Linux, `/dev/full` is writable as a device and
returns `ENOSPC`, preserving the behavior expected by shells and developer
tools; macOS does not provide `/dev/full`.

## Network

The supported modes are:

```toml
[network]
mode = "allow_audit"
```

```toml
[network]
mode = "deny_all"
```

```toml
[network]
mode = "allowlist"
allow = ["registry.npmjs.org:443", "127.0.0.1:3000"]
```

`allow_audit` permits unrestricted networking. `deny_all` installs no network
permission, so outbound connect and bind/listen are denied. On Linux,
`allowlist` installs one Sandlock rule per entry. On macOS, literal loopback
entries become Seatbelt rules; domain names and remote IPs use a
session-local, randomly authenticated HTTP/HTTPS CONNECT proxy. The sandbox
can reach that proxy but cannot open a direct unlisted connection. Proxy-aware
tools such as curl, npm, pip and Cargo work with hostname entries; a program
that ignores standard proxy variables remains network-denied.

An empty list is a valid deny-all allowlist. The legacy form
`mode = { allowlist = ["example.com:443"] }` remains accepted.

Allowlist entries must be non-empty and contain no control characters. `*` is
rejected in an allowlist; use `allow_audit` when unrestricted access is intended.
Localhost is not implicitly privileged and must be listed explicitly. macOS
normalizes `localhost`, `127.0.0.0/8` and `::1` spellings to Seatbelt's
`localhost` filter while retaining the exact listed port.

## Execution profiles

The policy above is the session's *standing* policy. One command can request a
different filesystem breadth with an execution profile:

| Profile | Writes reachable | Typical use |
|---|---|---|
| `workspace-write` (default) | The session workspace only | Editing the project |
| `read-write` | The workspace plus the rest of the host filesystem | `apt install`, toolchain and package-cache writes |

```bash
# Standing profile: the workspace, with the backing project unreachable.
deltabox exec "$SESSION" --project /abs/path/project -- sh -c 'echo hello > notes.txt'

# One command only: reach the host filesystem as well.
deltabox exec "$SESSION" --project /abs/path/project \
  --execution-profile read-write -- sh -c 'sudo apt-get install -y ripgrep'
```

The same field travels through every adapter: `execution_profile` on
`session.exec`, `--execution-profile` on the CLI (also accepted by
`deltabox run`), and the optional sixth argument of the Node binding's
`exec(sessionId, argv, cwd, environment, label, executionProfile)`.

Profiles widen access; they never relax the protected deny set. Under
`read-write`, the workspace still routes through Deltabox, so its changes stay
visible to diff, checkpoint, commit, and abort, while the backing project, the
session state root, and the helper endpoints remain denied. Two consequences
are worth stating explicitly:

- **Host changes outside the workspace are not rollback-able.** `read-write`
  exists for commands whose effect is inherently host-wide (`apt install`,
  service restarts, `~/.cache` population). Those writes are outside DeltaFS,
  so only the workspace portion of the turn can be reverted.
- **`read-write` needs the workspace outside the backing project.**
  `read-write` grants the filesystem root, so the backend keeps denying the
  backing project to preserve the workspace guarantee. A view nested inside
  that project cannot be expressed and the command fails closed with
  `unsupported` instead of exposing the host tree.

`git` isolation and the unsandboxed native profile reject `read-write`: neither
can widen host access without dropping the workspace guarantee.

## Environment

Every executed child starts with an empty environment. Deltabox copies only
`LANG`, `LC_ALL`, `LC_CTYPE`, `LOGNAME`, `PATH`, `SHELL`, `TERM`, `TZ`, and
`USER`, then supplies sandbox-specific `HOME`, `TMPDIR`, `XDG_CACHE_HOME`, and
`XDG_CONFIG_HOME`. Values passed explicitly through `session.exec.env` or CLI
`--env KEY=VALUE` are added afterwards. Supervisor API keys and unrelated
variables are never inherited implicitly.

## Limits

```toml
[limits]
max_memory_bytes = 8589934592
max_processes = 32
max_open_files = 1024
max_cpu_percent = 80
max_write_bytes = 16777216
max_storage_bytes = 1073741824
```

All limit fields are optional in a custom policy. The default agent policy sets
32 processes, 1024 open files, 16 MiB per direct `fs.write`, and 1 GiB of
pending storage. Its memory and CPU limits are unset; Sandlock's memory limiter
is incompatible with V8 virtual-address-space reservation in representative
Node/npm workloads.

On macOS, `max_open_files` and `max_memory_bytes` map to process rlimits.
`max_processes` bounds allowlist-proxy concurrency but is not currently an OS
process-count limit because Darwin's `RLIMIT_NPROC` is per user rather than per
sandbox. `max_cpu_percent` is not currently enforced on macOS. Storage and
direct-write limits are platform-neutral and remain fully enforced.

`max_write_bytes` applies to one direct write request. `max_storage_bytes` is
the aggregate size of regular files added or modified relative to the accepted
baseline. Direct writes and strict macOS sandbox execs check it before
returning. Fast native macOS execs defer the check to the next status, diff, or
commit boundary so the command path does not wait for FSEvents reconciliation.
An over-limit boundary returns an error; the pending changes remain available
for review, reduction, or abort.
