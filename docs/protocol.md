# deltabox-lite JSON-RPC protocol (v0.1)

The core exposes a line-delimited JSON-RPC 2.0-style protocol over stdio
(`deltabox serve`). Each request is one JSON object per line;
each response is one JSON object per line.

## Request

```json
{"id": 1, "method": "session.start", "params": {"project": "/abs/path", "policy": null, "fs_mode": "cbcow-through", "require_fs_mode": false, "durability": "strict", "group_commit_ms": 2}}
```

## Response

```json
{"id": 1, "result": {"session_id": "..."}}
{"id": 2, "error": {"code": -32000, "message": "session not found: x"}}
```

## Methods

| Method | Params | Result |
|---|---|---|
| `session.start` | `{project, policy?, fs_mode?, require_fs_mode?, durability?, group_commit_ms?}` | `SessionInfo` |
| `session.stop` | `{session_id}` | `SessionInfo` |
| `session.status` | `{session_id}` | `SessionInfo` |
| `session.exec` | `{session_id, argv, cwd?, env?, label?}` | `ExecResult` |
| `checkpoint.create` | `{session_id, label?}` | `CheckpointInfo` |
| `diff.list` | `{session_id, from?, to?}` | `DiffResult` |
| `changes.commit` | `{session_id, checkpoint?}` | `CommitResult` |
| `changes.abort` | `{session_id, checkpoint?}` | `AbortResult` |
| `review.begin` | `{session_id, kind, label?}` | `ActiveReview` |
| `review.status` | `{session_id}` | `ActiveReview \| null` |
| `review.diff` | `{session_id}` | `DiffResult` |
| `review.commit` | `{session_id}` | `CommitResult` |
| `review.abort` | `{session_id}` | `AbortResult` |
| `review.skip` | `{session_id}` | `ActiveReview` |

`fs_mode` accepts `cbcow-through`, `cbcow-private`, or `copy-private`. On Linux,
omitting it requests `cbcow-through`; a clean CB-CoW startup failure may resolve
to `copy-private`. Set `require_fs_mode: true` to reject fallback.

`durability` accepts `fast` (the default) or `strict`. Strict operations wait
for ordered storage barriers. `group_commit_ms` is optional in strict mode and
batches concurrent `APPLIED` journal barriers without allowing an operation to
return before its own group is synchronized. Strict mode requires
`cbcow-through` or `cbcow-private`; it rejects `copy-private` and does not use
automatic filesystem fallback.

`SessionInfo` = `{session_id, project, started_at_ms, status, checkpoint_count,
pending_changes, requested_fs_mode, resolved_fs_mode, host_visibility,
durability, group_commit_ms?, isolation, fuse_provider, fallback_reason, rollback_guarantee,
tainted, mountpoint?, helper_pid?, helper_restarts, mount_namespace}`.

For `cbcow-through`, `host_visibility` is `immediate`: commit accepts the
current host-visible state and abort reverts it. `cbcow-private` and
`copy-private` report `on-commit`; the former uses a private DeltaFS `LIVE` tree
and touched-path publication. The helper PID may change after automatic
recovery while the session and its durable state remain valid.

`ExecResult` = `{exit_code, stdout, stderr, duration_ms, truncated}`.

`label` on `session.exec` is optional free-form attribution (e.g. the Pi tool
name); it is recorded in the session journal so change sets can be grouped by
the command that produced them.

`DiffResult` = `{from, to, pending_changes, changes}` where each `Change` is
`{path, kind: added|modified|deleted, old_size, new_size, is_binary,
added_lines, removed_lines, text_diff?}`.

This is the stable protocol for adapters (`pi-deltabox`, MCP server, wrappers).
The same operations are available as independent CLI commands; `serve` is an
optional transport over the same durable state and may be restarted between
requests.
