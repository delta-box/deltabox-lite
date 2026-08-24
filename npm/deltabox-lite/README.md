# deltabox-lite

This package installs the native `deltabox` runtime for supported Linux and
macOS systems. npm exposes the matching optional platform package as the
`deltabox` command and as a Node-API addon. No JavaScript launcher or
prestarted service is installed.

```js
const { binaryPath, loadNative } = require('deltabox-lite')
const { NativeDeltabox } = loadNative()
const runtime = new NativeDeltabox(process.cwd(), binaryPath(), {
  fsMode: 'cbcow-through',
  isolation: 'deltabox',
})
```

Native methods return promises backed by Node's worker pool. File reads and
writes use `Buffer`, while session, exec, and review calls use typed objects.

Native options default to `cbcow-through` storage with Deltabox command
isolation. Supported pairs are `cbcow-through+deltabox`,
`cbcow-through+none`, and `git+bubblewrap`; invalid pairs fail at construction.

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Deltabox otherwise reports the reason and falls back to `copy-private`
unless the requested mode is required.

See the [deltabox-lite repository](https://github.com/Ddnirvana/deltabox-lite)
for usage and policy documentation.
