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

`/dev/null` is writable. `/dev/full` is writable as a device and returns
`ENOSPC`, which preserves the behavior expected by shells and common developer
tools.

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

`allow_audit` maps to Sandlock's unrestricted `*` network rule. `deny_all`
installs no network rule, so outbound connect and bind/listen are denied.
`allowlist` installs one Sandlock rule for each entry; an empty list is a valid
deny-all allowlist. The legacy form
`mode = { allowlist = ["example.com:443"] }` remains accepted.

Allowlist entries must be non-empty and contain no control characters. `*` is
rejected in an allowlist; use `allow_audit` when unrestricted access is intended.
Localhost is not implicitly privileged and must be listed explicitly.

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

`max_write_bytes` applies to one direct write request. `max_storage_bytes` is
the aggregate size of regular files added or modified relative to the accepted
baseline. It is checked after both direct writes and sandbox execs. Exceeding a
limit rejects the whole operation without publishing partial working state.
