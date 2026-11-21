# deltabox-lite

This package installs the native `deltabox` runtime for supported Linux, macOS,
and Windows systems. Windows requires WinFsp. The matching optional platform
package carries the binary and
the Node-API addon; `bin/deltabox.js` is a thin launcher that executes that
binary in place, so `npm install --global deltabox-lite` puts a `deltabox`
command on the PATH. No prestarted service is installed.

```js
const { binaryPath, loadNative } = require('deltabox-lite')
const { NativeDeltabox } = loadNative()
const runtime = new NativeDeltabox(process.cwd(), binaryPath(), {
  fsMode: 'cbcow-through',
  isolation: 'deltabox',
})
```

Native methods return promises backed by the addon's Tokio blocking runtime.
The coordinator and active filesystem state stay resident in the Node process;
storage mode and command isolation are fixed at construction. The default
`cbcow-through` + `deltabox` pair runs commands under the session's own sandbox
and deny set, which is why `isolation: 'none'` has to be asked for by name:
those commands share the Node process ambient environment instead. File reads
and writes use `Buffer`, while session, checkpoint, diff, commit, abort, and
review calls return protocol-shaped typed objects. Every session record names
its `isolation` profile and the resolved `isolation_provider` (`sandlock`,
`seatbelt`, or Windows MXC).

## Running commands

`spawn` returns a handle shaped like Node's `ChildProcess`, with real
`stdout`/`stderr` streams you can read while the command runs:

```js
const { spawn, exec } = require('deltabox-lite')

const child = spawn(runtime, sessionId, { argv: ['hvigorw', 'assembleHap'] })
child.stdout.on('data', (chunk) => process.stdout.write(chunk))
child.stderr.on('data', (chunk) => process.stderr.write(chunk))
const result = await child.result // exit_code, background_terminated, ...
```

- The command runs exactly as `exec` would: same session lock, sandbox,
  journaling and review tracking. Only its output goes to the streams instead
  of the result.
- The streams have backpressure: a reader that falls behind slows the command
  down instead of buffering its output in memory. Read both streams, or await
  `result`, which drains any stream nobody is reading.
- `child.kill()` ends the command's whole process tree. Destroying a stream
  makes the command's next write to it fail, as with any closed pipe.
- Events: `exit` (code) when the command has finished, then `close` (code)
  once both streams have closed too.
- When the command's main process exits, anything it left running in the
  background (`cmd &`) is stopped, so it cannot hold the streams open or
  outlive the command. `result.background_terminated` reports how many.
- Works under Node and Bun.

`exec` runs a command to completion and returns its buffered output. It takes
an options object, and an `AbortSignal` ends the command's process tree:

```js
const result = await exec(runtime, sessionId, {
  argv: ['npm', 'test'],
  env: { CI: '1' },
  signal: AbortSignal.timeout(600_000),
})
```

A command has no time limit: it runs until it exits or the host cancels it.
The native `exec(sessionId, argv, ...)` with an `execId` and
`cancelExec(execId)` remains available.

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Deltabox otherwise reports the reason and falls back to `copy-private`
unless the requested mode is required.

See the [deltabox-lite repository](https://github.com/delta-box/deltabox-lite)
for usage and policy documentation.
