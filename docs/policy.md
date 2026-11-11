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

### Execution-scoped filesystem grants

A command can request exact extra paths without changing the session. This is
on by default; a session that must never widen beyond its standing policy opts
out:

```toml
[filesystem]
allow_approval = false
```

| Field | Access | Rollback |
|---|---|---|
| `fs_read_allow` | read and execute (rx) of one exact path | nothing is written |
| `fs_write_allow` | read, write and execute (rwx) of a **directory tree**: create, write, rename and delete files and directories in it | **not rollback-able**: it is outside the workspace |

An rwx grant is always a directory, because creating, renaming or deleting a
file changes its directory, and atomic writes (a temporary sibling renamed over
the target, as `sed -i`, editors and package managers do) need that too.
Landlock and Windows ACLs cannot grant "only this name" in a directory. The
requested path is resolved like this:

| `fs_write_allow` names | Granted directory |
|---|---|
| an existing directory `/home/me/out` | `/home/me/out` |
| an existing file `/home/me/out/config.json` | `/home/me/out` |
| a new file `/home/me/out/new.txt` | `/home/me/out` |
| a path under missing directories `/home/me/.cache/hvigor/x.bin` | `/home/me/.cache/hvigor`, created (with missing parents) once every grant in the request is accepted |

The last component is always treated as the file to create, so to grant a new
directory itself, name a path inside it (`.../hvigor/x`).

The CLI flags are `--fs-read-allow PATH` and `--fs-write-allow PATH`
(repeatable); `session.exec` takes `fs_read_allow` and `fs_write_allow`; the
Node binding takes `fsReadAllow` and `fsWriteAllow` after `execId`.

Rules:

- **Exact, absolute paths.** Relative paths, `..`, wildcards, and the
  filesystem root are rejected (use the `read-write` execution profile for
  full host access). At most 32 grants per execution.
- **Not too broad.** A write grant whose directory is the root, a top-level
  directory (`/home`, `/etc`, `C:\Users`) or the user's home directory itself
  is rejected; grant a directory inside it.
- **Resolved before use.** Symlinks are resolved and the grant names the real
  target. A read grant must exist.
- **Nothing is created on refusal.** Missing directories for write grants are
  created only after every grant in the request passes every check.
- **Deny wins.** A grant whose path or granted directory matches a
  `filesystem.deny` pattern is rejected, and the protected deny set (engine
  state, the backing project, helper endpoints) still wins at the backend even
  under a grant. Deny patterns are matched against the grant, not against each
  file a command later creates inside a granted directory: the backends enforce
  name-based denies only inside the project.
- **No persistence.** Grants apply to a cloned policy for that one command.
- **Write implies read.** An rwx grant also adds the path to the read list.
- **Read includes execute.** The backends' `read` list already carries
  executable system paths (`/usr`, `/bin`), so an rx grant lets a command run
  binaries under it.
- **On by default.** With `allow_approval = false` any grant is rejected.
- **Backend support.** The same rx/rwx semantics on every platform:
  - **Linux** (FUSE `cbcow-*` modes and the durable copy mode): grants become
    Sandlock read/write rules.
  - **macOS** (Seatbelt): grants become `file-read*` / `file-write*` rules for
    the symlink-resolved path (so `/tmp/x` is granted as `/private/tmp/x`).
  - **Windows** (MXC PSEC, all modes): rx grants become `readonly_paths` and rwx
    grants `readwrite_paths`, as resolved host paths (the `\\?\` prefix from
    `canonicalize` is stripped). Deny patterns are matched with `/` separators and
    case-insensitively, so `C:\Users\me\.SSH` is still denied. Standing extra
    write roots in the policy file stay reserved on Windows; a per-command rwx
    grant is the explicit, non-rollback-able exception, as on the other platforms.
    **Limit:** PSEC grants write access without the DELETE right, so under an
    rwx grant a command can create, write, append, overwrite (`copy /y`) and
    create or remove empty directories, but cannot delete, rename or move a
    file, including one it created. Tools that replace files by renaming a
    temporary file over them fail there. This is PSEC's file policy, not
    something deltabox-lite can widen; the Windows tests assert it.
    The end-to-end tests need PSEC and WinFsp, so they are ignored by default
    and run in CI on windows-11-arm.
  - `crates/deltabox-cli/tests/fs_grants.rs` checks the same cases on every
    platform and session mode: create, mkdir, atomic replace, rename and delete
    under a write grant; siblings, read-only grants, secret and too-broad
    directories, refused sessions and the next exec stay closed.
  - Unsandboxed (`trusted`/`none`) execution rejects grants on every platform.

The Linux backend always lets commands read `/tmp`, so a read grant under `/tmp`
adds nothing there.

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
that ignores standard proxy variables remains network-denied. On Windows,
`deny_all` and non-loopback IP literals become MXC process-security-environment
egress rules; hostname and loopback entries use the same authenticated loopback
proxy as macOS, which keeps loopback access exact to the listed port. A list
served by the proxy reaches its IP-literal entries through the proxy as well.

An empty list is a valid deny-all allowlist. The legacy form
`mode = { allowlist = ["example.com:443"] }` remains accepted.

Allowlist entries must be non-empty and contain no control characters. `*` is
rejected in an allowlist; use `allow_audit` when unrestricted access is intended.
Localhost is not implicitly privileged and must be listed explicitly. macOS
normalizes `localhost`, `127.0.0.0/8` and `::1` spellings to Seatbelt's
`localhost` filter while retaining the exact listed port.

### Execution-scoped network grants

Sessions can opt into just-in-time, per-command network approvals by enabling
`allow_approval = true` in their standing policy:

```toml
[network]
mode = "allowlist"
allow_approval = true
allow = ["registry.npmjs.org:443"]     # Optional: static standing allowlist
deny = ["blocked.example:443"]         # Optional: prohibited destinations
allow_private = false                  # Optional: block LAN / loopback / metadata (default: false)
```

When enabled, the platform sandbox blocks direct egress at the kernel level and
restricts outbound traffic to an execution-scoped authenticated HTTP/CONNECT proxy.

#### Dynamic approvals (API)

A trusted orchestrator can grant temporary network access for a single command:
- **JSON-RPC (`serve`)**: Pass `network_allow: ["example.com:443"]` in `session.exec`.
- **Node.js binding**: Pass the optional `networkAllow` array to native `exec()`.

**Constraints & rules:**
- **Exact targets**: Destinations must be exact `host:port` (or `[ipv6]:port`). Wildcards (`*`), URLs, and paths are rejected.
- **Quota**: At most 32 targets per execution.
- **Deny precedence**: Targets listed in standing `deny` cannot be granted and return an error immediately.
- **No persistence**: Dynamic grants apply only to a cloned policy for that turn and are never persisted to the session.
- **Unsandboxed execution**: Trusted/`none` execution mode rejects approval-enabled policies and grants.

#### Enforcement & Security Model

- **Direct egress blocked**: Sandbox backends (Linux Landlock, macOS Seatbelt) permit egress exclusively to the local loopback proxy (`127.0.0.1:<port>`). Direct raw TCP/UDP out to other addresses is denied by the OS kernel.
- **Proxy environment**: The command helper automatically injects proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and empty `NO_PROXY`) with per-execution random Basic Auth credentials.
- **Private IP guard (`allow_private = false`)**: The proxy resolves DNS on the host and validates that the resolved IP is public before connecting. Connections use the checked IP directly without a second lookup (preventing DNS rebinding). Requests resolving to private, loopback, link-local, or cloud metadata addresses (e.g. `169.254.169.254`) are blocked.
- **Lifecycle cleanup**: The helper terminates the confined runtime and tears down all tracked proxy sockets immediately after each command. No connections or credentials survive across commands.
- **Protocol scope**: Supports HTTP and HTTPS via CONNECT tunnels. Direct TCP, UDP, QUIC (HTTP/3), and raw SSH are not proxied and remain blocked.

#### Denial reporting

Rejections enforced by the proxy are reported in the execution result under `network_denials`:

```json
{
  "exit_code": 1,
  "network_denials": [
    { "target": "example.com:443", "reason": "not-authorized" }
  ]
}
```

| Reason | Meaning |
|---|---|
| `not-authorized` | Target was not present in standing `allow` or execution `network_allow`. |
| `policy-denied` | Target matched standing `deny`. |
| `private-address` | Target resolved to a private/local IP while `allow_private = false`. |

*(Note: OS-level socket rejections and ordinary DNS/network failures do not generate proxy denial events.)*

#### Capability probe & Compatibility

Probe protocol capability before issuing network grants:
- **RPC**: Call `network.capabilities` (returns `{"version": 1}`).
- **CLI**: Run `deltabox network-capabilities`.
- **Node.js**: Call `native.networkApprovalsVersion()`.

**Platform notes:**
- **Linux & macOS**: Fully enforced via Landlock/Seatbelt and the internal proxy.
- **Windows**: MXC PSEC currently enforces filesystem boundaries only; network policy is unenforced and reported as a `degraded_protection` in session status.
- **Legacy static allowlists**: Policies with `allow_approval = false` retain their previous behavior without token authentication, strict private IP validation, or execution-scoped teardown.


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
visible to diff, checkpoint, commit, and abort. Engine state and helper
endpoints remain denied. Linux FUSE and private-workspace modes also deny
direct backing-project access; macOS through mode tracks writes in the real
project itself. The following rules apply:

- **The command's `HOME` and `TMPDIR` are session scratch, not host paths.**
  They stay writable under every profile, `read-write` included: widening a
  command's reach must not take away the private space it writes in.
- **Host changes outside the workspace are not rollback-able.** `read-write`
  exists for commands whose effect is inherently host-wide (`apt install`,
  service restarts, `~/.cache` population). Those writes are outside DeltaFS,
  so only the workspace portion of the turn can be reverted.
- **Isolated views must stay outside the denied backing project.**
  Linux FUSE and private-workspace modes retain the backing-project deny under
  `read-write`. A view inside that denied tree is rejected with `unsupported`.
  macOS `cbcow-through` instead uses the project as its tracked workspace, so
  it allows project access under both profiles. Its accepted baseline, session
  metadata, authentication tokens, and helper endpoints remain protected;
  only the private LIVE tree and command HOME/TMPDIR are exempt within state.
  Through-mode writes are immediately visible in the project; abort restores
  the baseline under the existing exclusive-writer contract.

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
