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

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Deltabox otherwise reports the reason and falls back to `copy-private`
unless the requested mode is required.

See the [deltabox-lite repository](https://github.com/delta-box/deltabox-lite)
for usage and policy documentation.
