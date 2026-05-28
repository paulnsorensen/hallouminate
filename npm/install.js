#!/usr/bin/env node

"use strict";

const https = require("https");
const http = require("http");
const fs = require("fs");
const path = require("path");
const { spawn } = require("child_process");

// cargo-dist artifact target triples. Hallouminate's dist-workspace.toml
// builds gnu (not musl) for Linux and does not target Windows.
const PLATFORM_MAP = {
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
};

const key = `${process.platform}-${process.arch}`;
const target = PLATFORM_MAP[key];

if (!target) {
  console.error(`hallouminate: unsupported platform ${key}`);
  console.error(`Supported: ${Object.keys(PLATFORM_MAP).join(", ")}`);
  console.error("Install manually: cargo install hallouminate");
  process.exit(1);
}

const version = require("./package.json").version;
const binName = "hallouminate";
// cargo-dist default unix archive format is .tar.xz; archive contains a
// top-level <app>-<target>/ directory holding the binary + README + LICENSE.
const archive = `hallouminate-${target}.tar.xz`;
const url = `https://github.com/paulnsorensen/hallouminate/releases/download/v${version}/${archive}`;

const binDir = path.join(__dirname, "bin");
const binPath = path.join(binDir, binName);

if (fs.existsSync(binPath)) {
  process.exit(0);
}

fs.mkdirSync(binDir, { recursive: true });

console.log(`hallouminate: downloading ${target} binary...`);

function follow(url, cb) {
  const mod = url.startsWith("https") ? https : http;
  mod
    .get(url, { headers: { "User-Agent": "hallouminate-npm" } }, (res) => {
      if (
        res.statusCode >= 300 &&
        res.statusCode < 400 &&
        res.headers.location
      ) {
        follow(res.headers.location, cb);
      } else if (res.statusCode !== 200) {
        console.error(`hallouminate: download failed (HTTP ${res.statusCode})`);
        console.error(`URL: ${url}`);
        console.error("Install manually: cargo install hallouminate");
        process.exit(1);
      } else {
        cb(res);
      }
    })
    .on("error", (err) => {
      console.error(`hallouminate: download failed: ${err.message}`);
      console.error("Install manually: cargo install hallouminate");
      process.exit(1);
    });
}

follow(url, (res) => {
  // tar -xJ understands xz on both macOS (BSD tar) and Linux (GNU tar).
  // --strip-components=1 flattens the top-level <app>-<target>/ wrapper so
  // the binary lands directly in npm/bin/.
  const tar = spawn(
    "tar",
    ["-xJ", "--strip-components=1", "-C", binDir],
    { stdio: ["pipe", "inherit", "inherit"] },
  );
  res.pipe(tar.stdin);
  tar.on("close", (code) => {
    if (code !== 0) {
      console.error(
        "hallouminate: failed to extract. Install manually: cargo install hallouminate",
      );
      process.exit(1);
    }
    if (!fs.existsSync(binPath)) {
      console.error(
        `hallouminate: binary missing after extract (expected ${binPath}).`,
      );
      console.error("Install manually: cargo install hallouminate");
      process.exit(1);
    }
    fs.chmodSync(binPath, 0o755);
    console.log("hallouminate: installed successfully");
  });
});
