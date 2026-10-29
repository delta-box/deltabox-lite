const { dirname, join } = require("node:path");

const packages = {
  "darwin-arm64": "deltabox-lite-darwin-arm64",
  "darwin-x64": "deltabox-lite-darwin-x64",
  "linux-arm64": "deltabox-lite-linux-arm64",
  "linux-x64": "deltabox-lite-linux-x64",
  "win32-x64": "deltabox-lite-windows-x64",
};

function platformPackageRoot() {
  const name = packages[`${process.platform}-${process.arch}`];
  if (!name) throw new Error(`deltabox-lite does not support ${process.platform}/${process.arch}`);
  return dirname(require.resolve(`${name}/package.json`));
}

function binaryPath() {
  return join(platformPackageRoot(), "bin", process.platform === "win32" ? "deltabox.exe" : "deltabox");
}

function nativePath() {
  return join(platformPackageRoot(), "native", "deltabox.node");
}

function loadNative() {
  return require(nativePath());
}

module.exports = { binaryPath, nativePath, loadNative };
