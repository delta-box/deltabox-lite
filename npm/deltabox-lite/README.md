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

Native methods return promises backed by the addon's Tokio blocking runtime.
The coordinator and active filesystem state stay resident in the Node process;
storage mode and command isolation are fixed at construction. The default
`cbcow-through` + `deltabox` pair runs commands under the session's own sandbox
and deny set, which is why `isolation: 'none'` has to be asked for by name:
those commands share the Node process ambient environment instead. File reads
and writes use `Buffer`, while session, checkpoint, diff, commit, abort, and
review calls return protocol-shaped typed objects. Every session record names
its `isolation` profile and the resolved `isolation_provider` (`sandlock` or
`seatbelt`).

Native options default to `cbcow-through` storage with Deltabox command
isolation. Supported pairs are `cbcow-through+deltabox`,
`cbcow-through+none`, and `git+bubblewrap`; invalid pairs fail at construction.
Session records expose the stable `isolation` profile and the resolved
`isolation_provider` (`sandlock`, `seatbelt`, or `bubblewrap`). Git mode uses
native `Buffer` reads and writes, supports fast durability only, and leaves
files matched by the project's `.gitignore` outside its rollback contract.

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Deltabox otherwise reports the reason and falls back to `copy-private`
unless the requested mode is required.

See the [deltabox-lite repository](https://github.com/Ddnirvana/deltabox-lite)
for usage and policy documentation.
