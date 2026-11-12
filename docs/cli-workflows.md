# CLI workflows

These commands expose existing session, diff, policy, and network behavior in
the CLI. They retain each backend's enforcement and filesystem semantics; a CLI
option does not add support to a backend that lacks it. The commands below are
part of the current source tree; see [Unreleased](../CHANGELOG.md#unreleased).

Examples use a POSIX shell. Run project commands from the project directory or
pass `--project /abs/path/project`. If session state was relocated with
`DELTABOX_STATE_DIR`, keep that setting for later commands.

## Find a recorded session

Use `session list` when returning to a project or when a script needs to find a
session ID:

```bash
deltabox session list
deltabox session list --status running
deltabox session list --status stopped --project /abs/path/project --json
```

The table shows the session ID, recorded lifecycle status, resolved filesystem
mode, and start time in Unix milliseconds. Results are oldest first. JSON is an
array containing `session_id`, `project`, `started_at_ms`, `status`,
`resolved_fs_mode`, and `durability`; an empty catalog produces `[]`.

`running` and `stopped` are the last persisted lifecycle state. They do not mean
that a helper process has or has not survived. Listing reads session records
without recovering sessions, starting helpers, or calculating pending file
changes. Use `session status SESSION` when you need the session's existing
status/reconciliation behavior. This is a per-project catalog, not a search
through every project on the machine.

## End a session while preserving current edits

On macOS, `session release` ends a `cbcow-through` session while retaining the
current files in the host project. This is useful when host tools have edited
the project and you want to release the session's ownership without rolling
those edits back:

```bash
demo=$(mktemp -d)
cd "$demo"
printf 'original\n' > notes.txt
session_id=$(deltabox session start . --fs-mode cbcow-through --require-fs-mode)
printf 'edited on the host\n' > notes.txt
deltabox session release "$session_id"
cat notes.txt
```

The command prints the stopped session as JSON and releases its project
ownership. The recorded session remains available to `session list`. Release
does not commit a private workspace; files already visible in the project remain
as they are. Normal `session stop` retains its existing behavior of aborting
pending changes.

Release requires macOS `cbcow-through` and no active review scope. Private modes
and other platforms are unsupported. Complete an active review with its
appropriate commit or abort decision first; `review skip` keeps that review
active. A stopped session cannot be used for later exec, commit, or abort
operations; start a new session to continue working.

## Focus a diff on selected paths

Both `diff` and `review diff` accept the same output and path-selection options:

```bash
deltabox diff "$session_id" --path src --path README.md --stat
deltabox review diff "$session_id" --path src --name-only
```

| Option | Output |
|---|---|
| No output flag | Existing unified text diff with the usual binary/large-file markers. |
| `--stat` | Per-path added/deleted line counts, binary/large-file byte sizes, and a total for the selected files. |
| `--name-only` | One changed path per line, without file contents. |
| `--json` | The existing diff object, with `changes` and `pending_changes` restricted to the selected paths. |

`--stat`, `--name-only`, and `--json` are mutually exclusive. Paths containing
control characters, quotes, or backslashes are JSON-quoted in `--name-only` and
`--stat` output; use JSON output when consuming arbitrary filenames in a script.

Repeat `--path` to select the union of several paths. Each value is a literal,
project-relative file or subtree, using `/` separators. For example, `src`
matches `src` and `src/lib.rs`, but not `src2/lib.rs`. `./src/` is normalized to
`src`, and `.` selects the whole project. Absolute paths, Windows drive prefixes,
backslashes, and `..` components are rejected. Filtering is lexical and also
works for deleted paths; it does not resolve symlinks or require a file to exist.
There is no glob expansion: a quoted `--path 'src/*'` means a literal path with
an asterisk, not every file in `src`.

**Filtering changes the display only.** It does not select files for a later
commit or abort; those commands retain their existing scope. An empty filtered
diff does not prove that the rest of the project is unchanged. Review diffs
still need an active review.

## Inspect and validate policy files

Use `policy check` to catch a bad policy before starting a session, and `policy
show` to inspect or save the values that the runtime will use:

```bash
# Create a starter with the default agent grants, denies, and limits.
deltabox policy show > policy.toml
deltabox policy check policy.toml
deltabox policy check policy.toml --json
deltabox policy show --policy policy.toml --json
```

`check FILE` rejects malformed TOML, invalid field types, unknown keys, and
violations of the existing policy validation rules. Unknown fields include their
full path, such as `network.allow_aproval`. Both the current allowlist syntax and
legacy `mode = { allowlist = [...] }` syntax work. Success exits zero; JSON success
contains `{"valid": true, "policy": ...}`. Errors exit nonzero with the cause on
stderr and do not emit a success object.

`show` prints canonical TOML; `--json` prints the policy object. With no file it
uses the default agent policy. An explicit `--policy FILE` uses the actual
deserialization defaults, **without merging the default agent policy**. For
example, a file containing only a `deny_all` network section has empty filesystem
grant/deny arrays and unset limits. To customize the usual agent policy, start
with `policy show > policy.toml`, then edit that generated file.

`show --policy` matches the runtime's existing permissive handling of unknown
fields; run `check` for strict typo detection. Canonical TOML expands defaults
and rewrites legacy allowlists into the current syntax, while retaining the same
policy values. Comments and original formatting are not retained.

Neither command creates sessions or state directories. Validation does not test
filesystem access, contact network destinations, or guarantee enforcement of
every resource limit on every platform. See the [policy reference](policy.md)
for the schema and backend-specific limits.

## Grant network access for one command

`--network-allow HOST:PORT` gives a command temporary access to exact network
destinations under an approval-enabled policy. A harness can use this to permit
one download while leaving the session's standing allowlist unchanged.

First generate a default policy:

```bash
deltabox policy show > approval-policy.toml
```

Replace its existing `[network]` section with:

```toml
[network]
mode = "allowlist"
allow = []
allow_approval = true
deny = ["blocked.example:443"]
allow_private = false
```

After checking the file, start a session and grant a destination for one exec:

```bash
deltabox policy check approval-policy.toml
session_id=$(deltabox session start . --policy approval-policy.toml)
deltabox exec "$session_id" --network-allow example.com:443 -- \
  curl --fail --silent --show-error https://example.com/
```

Repeat the flag for additional destinations:

```bash
deltabox exec "$session_id" \
  --network-allow example.com:443 --network-allow example.org:443 -- \
  curl --fail --silent --show-error https://example.com/ https://example.org/
```

After finishing the persistent session, stop it before starting a temporary
session in the same project. Pass the policy and choose the filesystem decision:

```bash
deltabox session stop "$session_id"
deltabox run --policy approval-policy.toml --network-allow example.com:443 \
  --abort -- curl --fail --silent --show-error https://example.com/

deltabox run --policy approval-policy.toml --network-allow example.com:443 \
  --commit -- curl --fail --silent --show-error \
  --output downloaded.html https://example.com/
```

The limits are the same as the existing RPC/native network grants:

- The policy must opt in with both `mode = "allowlist"` and
  `allow_approval = true`. The default agent policy does not opt in.
- Repeat `--network-allow` for up to 32 targets per command. Targets must be exact
  `host:port` or bracketed IPv6 such as `'[::1]:3000'`. Wildcards, URLs, and paths
  are rejected. Hostnames are normalized; a port is required.
- Standing `deny` entries still prohibit access. A grant is added only for this
  command and expires when it finishes, regardless of the `run` commit/abort
  decision. A later exec must request the grant again unless it is in standing
  `allow`.
- Loopback, LAN, link-local, and other private/local destinations also require
  `allow_private = true`. With the default `false`, DNS resolving to those
  addresses is blocked as well.
- Grants use the existing authenticated HTTP/HTTPS CONNECT proxy. Proxy-aware
  HTTP clients work; direct TCP/UDP, QUIC, and raw SSH are outside this interface.
  Redirects to another host/port require that destination to be allowed too.
- Enforcement uses the existing sandbox backend: Landlock on Linux, Seatbelt on
  macOS, and MXC on Windows. Trusted/unsandboxed execution rejects these grants;
  this CLI option does not relax that restriction.

These flags control network access only. `--abort` rolls back the command's
tracked filesystem changes; it cannot undo a request already sent to a server.
See [network policy](policy.md#execution-scoped-network-grants) for enforcement
details and API equivalents.
