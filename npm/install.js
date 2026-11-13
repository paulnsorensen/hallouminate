#!/usr/bin/env node

"use strict";

const https = require("https");
const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
const { PLATFORM_MAP, artifactFor } = require("./platform");
const { installArchive } = require("./extract");

const key = `${process.platform}-${process.arch}`;
const artifact = artifactFor(process.platform, process.arch);
const target = artifact?.target;

if (!target) {
  console.error(`hallouminate: unsupported platform ${key}`);
  console.error(`Supported: ${Object.keys(PLATFORM_MAP).join(", ")}`);
  console.error("Install manually: cargo install hallouminate");
  process.exit(1);
}

const version = require("./package.json").version;
const binName = artifact.binary;
const archive = artifact.archive;
const url = `https://github.com/paulnsorensen/hallouminate/releases/download/v${version}/${archive}`;

const binDir = path.join(__dirname, "bin");
const binPath = path.join(binDir, binName);

if (fs.existsSync(binPath)) {
  process.exit(0);
}

fs.mkdirSync(binDir, { recursive: true });

console.log(`hallouminate: downloading ${target} binary...`);

const MAX_REDIRECTS = 5;
const REQUEST_TIMEOUT_MS = 30_000;

function follow(url, redirectsLeft, cb) {
  // postinstall download: refuse anything but HTTPS, including redirect
  // targets, so a hijacked Location header can't downgrade or jump host.
  if (!url.startsWith("https:")) {
    console.error(`hallouminate: refusing non-HTTPS download URL: ${url}`);
    console.error("Install manually: cargo install hallouminate");
    process.exit(1);
  }
  const req = https
    .get(url, { headers: { "User-Agent": "hallouminate-npm" } }, (res) => {
      if (
        res.statusCode >= 300 &&
        res.statusCode < 400 &&
        res.headers.location
      ) {
        res.resume(); // drain the redirect body so the socket is freed
        if (redirectsLeft <= 0) {
          console.error(`hallouminate: too many redirects fetching ${url}`);
          console.error("Install manually: cargo install hallouminate");
          process.exit(1);
        }
        follow(res.headers.location, redirectsLeft - 1, cb);
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
  req.setTimeout(REQUEST_TIMEOUT_MS, () => {
    req.destroy(new Error(`timed out after ${REQUEST_TIMEOUT_MS}ms`));
  });
}

function downloadToBuffer(url, cb) {
  follow(url, MAX_REDIRECTS, (res) => {
    const chunks = [];
    res.on("data", (chunk) => chunks.push(chunk));
    res.on("end", () => cb(Buffer.concat(chunks)));
    res.on("error", (err) => {
      console.error(`hallouminate: download failed: ${err.message}`);
      console.error("Install manually: cargo install hallouminate");
      process.exit(1);
    });
  });
}

const shaUrl = `${url}.sha256`;

downloadToBuffer(shaUrl, (shaBuf) => {
  const expected = shaBuf.toString("utf8").trim().split(/\s+/)[0].toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(expected)) {
    console.error(`hallouminate: malformed checksum file at ${shaUrl}`);
    console.error("Install manually: cargo install hallouminate");
    process.exit(1);
  }

  downloadToBuffer(url, (tarBuf) => {
    const actual = crypto.createHash("sha256").update(tarBuf).digest("hex");
    if (actual !== expected) {
      console.error(`hallouminate: checksum mismatch for ${archive}`);
      console.error(`expected: ${expected}`);
      console.error(`actual:   ${actual}`);
      console.error("Install manually: cargo install hallouminate");
      process.exit(1);
    }

    installArchive(tarBuf, artifact, binDir).then(() => {
      console.log("hallouminate: installed successfully");
    }).catch((error) => {
      console.error(`hallouminate: ${error.message}`);
      console.error("Install manually: cargo install hallouminate");
      process.exitCode = 1;
    });
  });
});
