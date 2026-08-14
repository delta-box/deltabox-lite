# deltabox-client

A minimal Node.js SDK for the [deltabox-lite](https://github.com/Ddnirvana/deltabox-lite)
sandbox runtime. It spawns `deltabox serve` and speaks the line-delimited
JSON-RPC protocol documented in `docs/protocol.md`.

## Usage

```js
import { DeltaboxClient } from "deltabox-client";

const box = await DeltaboxClient.start("/abs/path/to/project");

await box.write("src/hello.py", 'print("hi")\n');
const out = await box.exec(["python3", "src/hello.py"]);

const diff = await box.diff();          // review pending changes
await box.abort();                      // ...or discard them
await box.write("src/hello.py", 'print("final")\n');
await box.commit();                     // accept into the real project

await box.stop();
```

`DeltaboxClient.start(project, opts)` accepts `binary` (path to the deltabox
executable, defaults to `deltabox` on PATH), `fsMode`, and `durability`.
Relative paths passed to `read`/`write` resolve against the project root.
`call(method, params)` is the escape hatch for any protocol method without a
typed helper.

## Test

Runs the full lifecycle against a real binary in a throwaway project:

```sh
DELTABOX_BIN=/path/to/deltabox npm test
```
