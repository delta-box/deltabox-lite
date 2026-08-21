# deltabox-lite

This package installs the native `deltabox` runtime for supported Linux and
macOS systems. npm exposes the matching optional platform package as the
`deltabox` command and as a Node-API addon. No JavaScript launcher or
prestarted service is installed.

```js
const { binaryPath, loadNative } = require('deltabox-lite')
const { NativeDeltabox } = loadNative()
const runtime = new NativeDeltabox(process.cwd(), binaryPath(), true)
```

Native methods return promises backed by the addon's Tokio blocking runtime.
The coordinator and active filesystem state stay resident in the Node process;
trusted commands share its ambient environment and reuse an isolated Bash
worker. File reads and writes use `Buffer`, while session, checkpoint, diff,
commit, abort, and review calls return protocol-shaped typed objects.

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Deltabox otherwise reports the reason and falls back to `copy-private`
unless the requested mode is required.

See the [deltabox-lite repository](https://github.com/Ddnirvana/deltabox-lite)
for usage and policy documentation.
