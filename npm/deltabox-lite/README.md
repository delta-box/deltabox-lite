# deltabox-lite

This package installs the native `deltabox` runtime for supported Linux and
macOS systems. npm exposes the matching optional platform package as the
`deltabox` command; no JavaScript launcher, FUSE package, or prestarted service
is installed.

Linux CB-CoW modes require `/dev/fuse` and `fusermount3` or `fusermount` on the
host. Deltabox otherwise reports the reason and falls back to `copy-private`
unless the requested mode is required.

See the [deltabox-lite repository](https://github.com/Ddnirvana/deltabox-lite)
for usage and policy documentation.
