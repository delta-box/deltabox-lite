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
trusted commands share its ambient environment and reuse an isolated Bash
worker. File reads and writes use `Buffer`, while session, checkpoint, diff,
commit, abort, and review calls return protocol-shaped typed objects.

Native callers must select one exact storage mode and command-isolation
profile. CB-CoW through/private and copy-private storage accept `deltabox` or
`none`; Git storage accepts `bubblewrap` or `docker`. Invalid pairs fail at
construction, and unavailable exact modes fail at session start without an
automatic fallback. Docker isolation is Linux-only and uses the fixed,
preinstalled `ubuntu:22.04` image; the addon never pulls it automatically and
the embedding process must have Docker daemon access.
Session records expose the stable `isolation` profile and the resolved
`isolation_provider` (`sandlock`, `seatbelt`, `bubblewrap`, or `docker`). Git
mode uses native `Buffer` reads and writes, supports fast durability only, and
leaves files matched by the project's `.gitignore` outside its rollback
contract.

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Select `copy-private` explicitly when those requirements are unavailable.

See the [deltabox-lite repository](https://github.com/Ddnirvana/deltabox-lite)
for usage and policy documentation.
