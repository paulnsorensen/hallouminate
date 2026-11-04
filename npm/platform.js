"use strict";

const PLATFORM_MAP = {
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
};

function artifactFor(platform, arch) {
  const target = PLATFORM_MAP[`${platform}-${arch}`];
  if (!target) return null;
  const windows = platform === "win32";
  return {
    target,
    archive: `hallouminate-${target}.${windows ? "zip" : "tar.xz"}`,
    binary: windows ? "hallouminate.exe" : "hallouminate",
  };
}

module.exports = { PLATFORM_MAP, artifactFor };
