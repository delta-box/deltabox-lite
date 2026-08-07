# macOS backend

Deltabox-lite uses the native macOS Seatbelt runtime through
`/usr/bin/sandbox-exec`. Each durable command runs directly in the transaction
layer's APFS-cloned working tree; it does not create a second backend shadow.
The optional long-running server is a transport over the same durable
per-command transactions. The real project is write-denied until `commit`
publishes the reviewed delta.

## Path mapping

The public `deltabox_core::pathmap::PathMapper` maps:

- `cwd` values under the real project;
- complete absolute argv values under the real project;
- complete absolute environment values; and
- components of `PATH`, `PYTHONPATH`, `PKG_CONFIG_PATH`, and related path lists.

It deliberately does not replace substrings in shell source, Python source,
configuration blobs, or other arbitrary text. Use relative paths or pass an
absolute project path as its own argument when portability matters. Hardcoded
real-project writes are denied rather than redirected.

## Compatibility status

We validated deltabox-lite with the following scenarios to check the compatibility:

| Capability | macOS behavior |
|---|---|
| Shell redirects, pipes, exit codes | Supported |
| `/dev/null`, randomness devices | Supported |
| Git, Node/npm, Python/pip/pytest | Supported |
| Rust/cargo, make/CMake | Supported; rustup toolchains are read-only |
| `rg`, `find`, tar/gzip/zip | Supported |
| Durable diff/commit/abort | Supported with APFS clones |
| Optional `serve` transport | Supported over the same durable per-command transactions |
| Network deny-all | Enforced by Seatbelt |
| Literal localhost allowlist | Direct Seatbelt rule for the listed port |
| Hostname/remote-IP allowlist | Authenticated local HTTP/CONNECT proxy |
| Raw sockets to an allowed hostname | Not proxied; use a proxy-aware client |
| Hardcoded absolute path inside source text | Not remapped; real-project writes denied |
| `max_open_files`, `max_memory_bytes` | Enforced with rlimits |
| `max_processes`, `max_cpu_percent` | Not enforced as OS process/CPU limits |

APFS clone support is detected per file. Deltabox-lite falls back to an
ordinary copy on filesystems where `clonefile` is unavailable.
