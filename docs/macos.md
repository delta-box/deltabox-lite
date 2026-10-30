# macOS backend

Deltabox-lite combines Seatbelt confinement with a native APFS/FSEvents state
provider. It does not require macFUSE, a system extension, root, Full Disk
Access, or a prestarted service. Independent CLI processes reopen the same
on-disk state.

`cbcow-through` runs directly in the project, so host changes are immediate.
An APFS-cloned accepted base supplies rollback state. `cbcow-private` runs in a
private APFS-cloned `LIVE` tree and conflict-checks touched host paths before
publishing them at commit. State is kept in a hidden sibling of the project by
default; `DELTABOX_STATE_DIR` overrides that location.

Both sandboxed native modes support the per-command `read-write` execution
profile. In through mode the real project remains writable and tracked; in
private mode commands write the LIVE tree and direct host-project access stays
denied. Both protect the accepted baseline, session metadata, command token,
and helper endpoints even when the filesystem root is writable. Private LIVE
and command HOME/TMPDIR remain usable within the protected state tree.
Project changes are reviewable and rollback-able; writes elsewhere on the host
are immediate and cannot be rolled back. Through-mode project writes are also
immediately visible, and retain the exclusive-writer contract described below.

Session start builds the accepted base, and the private `LIVE` tree, with one
APFS `clonefile` call per top-level project entry, so APFS copies whole
subtrees in the kernel. Such a directory clone resets directory times and drops
nested ACLs, set-id bits, group ownership, and hard links, so every entry is
then compared with a scan of the project and repaired where it differs. The
through-mode index reuses that scan. Start-up still reads every entry, but no
longer rewrites each one.

Each session starts a small FSEvents helper to retain the warm file-level event
stream across CLI processes. The helper owns no filesystem state: if it exits,
the next operation rebuilds it from the persisted event ID and device UUID.
Events identify paths to reconcile against the current tree; they are not
replayed as filesystem operations. Dropped events, event-ID wrap, root
replacement, or device identity change force a current-tree scan. Source-side
APFS clone notifications are ignored only when reconciliation proves the
indexed path is unchanged.

Fast `cbcow-through` assumes Deltabox is the exclusive project writer while a
session is active. Fast `exec` returns after the confined child exits; the next
status, diff, checkpoint, commit, abort, or stop operation closes a sentinel
boundary and persists event accounting before reading state. Strict sessions
close that boundary before every `exec` returns, mark historical external
writes as tainted, and refuse abort until those writes are accepted. Private
mode keeps its `LIVE` tree separate and conflict-checks touched host paths at
commit.

## Path mapping

The public `deltabox_core::pathmap::PathMapper` maps:

- `cwd` values under the real project;
- complete absolute argv values under the real project;
- complete absolute environment values; and
- components of `PATH`, `PYTHONPATH`, `PKG_CONFIG_PATH`, and related path lists.

It deliberately does not replace substrings in shell source, Python source,
configuration blobs, or other arbitrary text. Use relative paths or pass an
absolute project path as its own argument when portability matters. Hardcoded
real-project access is denied rather than redirected in private mode. The
`.deltabox` session metadata and native state tree are denied in both modes.

## Compatibility status

We validated deltabox-lite with the following scenarios to check the compatibility:

| Capability | macOS behavior |
|---|---|
| Shell redirects, pipes, exit codes | Supported |
| `/dev/null`, randomness devices | Supported |
| Git, Node/npm, Python/pip/pytest | Supported |
| Rust/cargo, make/CMake | Supported; rustup toolchains are read-only |
| `rg`, `find`, tar/gzip/zip | Supported |
| Cross-process diff/checkpoint/commit/abort | Supported with APFS clones and persistent FSEvents cursors |
| Optional `serve` transport | Supported over the same durable per-command transactions |
| Network deny-all | Enforced by Seatbelt |
| Literal localhost allowlist | Direct Seatbelt rule for the listed port |
| Hostname/remote-IP allowlist | Authenticated local HTTP/CONNECT proxy |
| Raw sockets to an allowed hostname | Not proxied; use a proxy-aware client |
| Hardcoded absolute path inside source text | Not remapped; original host tree denied in private mode |
| `max_open_files`, `max_memory_bytes` | Enforced with rlimits |
| `max_processes`, `max_cpu_percent` | Not enforced as OS process/CPU limits |

Required native modes probe `clonefile` between the project and state directory
at session start. If the requested mode is optional and the probe fails,
Deltabox reports the reason and resolves to `copy-private`; required mode fails.
macFUSE remains optional and is not used by the native provider.
