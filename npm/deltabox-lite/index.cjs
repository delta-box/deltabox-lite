const { dirname, join } = require("node:path");

const packages = {
  "darwin-arm64": "deltabox-lite-darwin-arm64",
  "darwin-x64": "deltabox-lite-darwin-x64",
  "linux-arm64": "deltabox-lite-linux-arm64",
  "linux-x64": "deltabox-lite-linux-x64",
};

function binaryPath() {
  const name = packages[`${process.platform}-${process.arch}`];
  if (!name) throw new Error(`deltabox-lite does not support ${process.platform}/${process.arch}`);
  return join(dirname(require.resolve(`${name}/package.json`)), "bin", "deltabox");
}

module.exports = { binaryPath };
