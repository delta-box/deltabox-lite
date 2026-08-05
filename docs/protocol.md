# deltabox-lite JSON-RPC protocol (v0.1)

The core exposes a line-delimited JSON-RPC 2.0-style protocol over stdio
(`deltabox serve`) or a Unix socket. Each request is one JSON object per line;
each response is one JSON object per line.

## Request

```json
{"id": 1, "method": "session.start", "params": {"project": "/abs/path", "policy": null}}
```

## Response

```json
{"id": 1, "result": {"session_id": "..."}}
{"id": 2, "error": {"code": -32000, "message": "session not found: x"}}
```

## Methods

| Method | Params | Result |
|---|---|---|
| `session.start` | `{project, policy?}` | `SessionInfo` |
| `session.stop` | `{session_id}` | `SessionInfo` |
| `session.status` | `{session_id}` | `SessionInfo` |
| `session.exec` | `{session_id, argv, cwd?, env?, label?}` | `ExecResult` |
| `checkpoint.create` | `{session_id, label?}` | `CheckpointInfo` |
| `diff.list` | `{session_id, from?, to?}` | `DiffResult` |
| `changes.commit` | `{session_id, checkpoint?}` | `CommitResult` |
| `changes.abort` | `{session_id, checkpoint?}` | `AbortResult` |

`SessionInfo` = `{session_id, project, started_at_ms, status, checkpoint_count,
pending_changes}`.

`ExecResult` = `{exit_code, stdout, stderr, duration_ms, truncated}`.

`label` on `session.exec` is optional free-form attribution (e.g. the Pi tool
name); it is recorded in the session journal so change sets can be grouped by
the command that produced them.

`DiffResult` = `{from, to, pending_changes, changes}` where each `Change` is
`{path, kind: added|modified|deleted, old_size, new_size, is_binary,
added_lines, removed_lines, text_diff?}`.

This is the stable contract for adapters (`pi-deltabox`, MCP server, wrappers).
