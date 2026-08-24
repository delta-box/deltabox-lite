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
`cbcow-through+none`, `git+bubblewrap`, and `git+docker`; invalid pairs fail at
construction. Docker isolation is Linux-only and uses the fixed, preinstalled
`ubuntu:22.04` image; the addon never pulls it automatically and the embedding
process must have Docker daemon access.
Session records expose the stable `isolation` profile and the resolved
`isolation_provider` (`sandlock`, `seatbelt`, `bubblewrap`, or `docker`). Git
mode uses native `Buffer` reads and writes, supports fast durability only, and
leaves files matched by the project's `.gitignore` outside its rollback
contract.

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Deltabox otherwise reports the reason and falls back to `copy-private`
unless the requested mode is required.

See the [deltabox-lite repository](https://github.com/Ddnirvana/deltabox-lite)
for usage and policy documentation.
