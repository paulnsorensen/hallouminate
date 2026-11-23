"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const crypto = require("node:crypto");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { test } = require("node:test");
const { artifactFor, PLATFORM_MAP } = require("../platform");
const { installArchive } = require("../extract");

const sha256 = (buffer) => crypto.createHash("sha256").update(buffer).digest("hex");

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", windowsHide: true, ...options });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  return result.stdout;
}

function makeArchive(root, artifact) {
  const source = path.join(root, "source");
  const payload = process.platform === "win32" ? source : path.join(source, "hallouminate-release");
  fs.mkdirSync(payload, { recursive: true });
  fs.writeFileSync(path.join(payload, artifact.binary), "executable fixture");
  fs.writeFileSync(path.join(payload, "onnxruntime.dll"), "runtime fixture");
  const archive = path.join(root, artifact.archive);
  if (process.platform === "win32") {
    run("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command",
      "$ErrorActionPreference = 'Stop'; Compress-Archive -Path (Join-Path $env:TEST_SOURCE '*') -DestinationPath $env:TEST_ARCHIVE"],
      { env: { ...process.env, TEST_SOURCE: source, TEST_ARCHIVE: archive } });
  } else {
    run("tar", ["-cJf", archive, "-C", source, "hallouminate-release"]);
  }
  return archive;
}

test("npm platforms match the release targets", () => {
  const config = fs.readFileSync(path.join(__dirname, "../../dist-workspace.toml"), "utf8");
  const targets = JSON.parse(config.match(/^targets = (.+)$/m)[1]);
  assert.deepEqual(Object.values(PLATFORM_MAP).sort(), targets.sort());
  assert.equal(artifactFor("win32", "arm64"), null);
});

test("native archive extraction installs the executable and adjacent runtime files", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "hallouminate package "));
  try {
    const artifact = artifactFor(process.platform, process.arch);
    assert.ok(artifact);
    const archive = makeArchive(root, artifact);
    const destination = path.join(root, "installation with 'quotes'", "bin");
    await installArchive(fs.readFileSync(archive), artifact, destination);
    assert.equal(fs.readFileSync(path.join(destination, artifact.binary), "utf8"), "executable fixture");
    assert.equal(fs.readFileSync(path.join(destination, "onnxruntime.dll"), "utf8"), "runtime fixture");
    if (process.platform !== "win32") assert.ok(fs.statSync(path.join(destination, artifact.binary)).mode & 0o111);

    const missing = { ...artifact, binary: "missing-binary" };
    await assert.rejects(installArchive(fs.readFileSync(archive), missing, path.join(root, "missing")), /binary missing/);
    assert.equal(fs.existsSync(path.join(root, "missing")), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("launcher preserves arguments and child exit status", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "hallouminate-launcher-"));
  try {
    const binary = artifactFor(process.platform, process.arch).binary;
    fs.mkdirSync(path.join(root, "bin"));
    fs.copyFileSync(process.execPath, path.join(root, "bin", binary));
    fs.chmodSync(path.join(root, "bin", binary), 0o755);
    fs.copyFileSync(path.join(__dirname, "../run.js"), path.join(root, "run.js"));
    const args = ["space and 'quote'", 'double"quote', "$literal"];
    const stdout = run(process.execPath, [path.join(root, "run.js"), "-e", "console.log(JSON.stringify(process.argv.slice(1)))", ...args]);
    assert.deepEqual(JSON.parse(stdout), args);
    const result = spawnSync(process.execPath, [path.join(root, "run.js"), "-e", "process.exit(23)"], { windowsHide: true });
    assert.equal(result.status, 23);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

// Preload that replaces https.get so install.js runs as a real entrypoint
// against a local archive and checksum instead of GitHub.
const FAKE_HTTPS = `
const https = require("node:https");
const { EventEmitter } = require("node:events");
const { Readable } = require("node:stream");
const fs = require("node:fs");
https.get = (url, _options, callback) => {
  const req = new EventEmitter();
  req.setTimeout = () => {};
  req.destroy = () => {};
  const file = url.endsWith(".sha256") ? process.env.FAKE_SHA_FILE : process.env.FAKE_ARCHIVE_FILE;
  setImmediate(() => {
    const res = Readable.from([fs.readFileSync(file)]);
    res.statusCode = 200;
    res.headers = {};
    callback(res);
  });
  return req;
};
`;

function runInstaller(root, archiveBuffer, checksum) {
  const artifact = artifactFor(process.platform, process.arch);
  const pkg = path.join(root, "pkg");
  fs.mkdirSync(pkg);
  for (const file of ["install.js", "extract.js", "platform.js", "package.json"]) {
    fs.copyFileSync(path.join(__dirname, "..", file), path.join(pkg, file));
  }
  const preload = path.join(root, "fake-https.js");
  const archiveFile = path.join(root, "served-archive");
  const shaFile = path.join(root, "served-sha");
  fs.writeFileSync(preload, FAKE_HTTPS);
  fs.writeFileSync(archiveFile, archiveBuffer);
  fs.writeFileSync(shaFile, `${checksum}  ${artifact.archive}\n`);
  const result = spawnSync(process.execPath, ["-r", preload, path.join(pkg, "install.js")], {
    encoding: "utf8",
    windowsHide: true,
    env: { ...process.env, FAKE_ARCHIVE_FILE: archiveFile, FAKE_SHA_FILE: shaFile },
  });
  assert.ifError(result.error);
  return { result, binary: path.join(pkg, "bin", artifact.binary) };
}

test("installer entrypoint installs an archive whose checksum matches", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "hallouminate-installer-"));
  try {
    const artifact = artifactFor(process.platform, process.arch);
    const buffer = fs.readFileSync(makeArchive(root, artifact));
    const { result, binary } = runInstaller(root, buffer, sha256(buffer));
    assert.equal(result.status, 0, result.stderr);
    assert.equal(fs.readFileSync(binary, "utf8"), "executable fixture");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("installer entrypoint rejects a checksum mismatch without installing a binary", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "hallouminate-installer-"));
  try {
    const artifact = artifactFor(process.platform, process.arch);
    const buffer = fs.readFileSync(makeArchive(root, artifact));
    const { result, binary } = runInstaller(root, buffer, sha256(Buffer.from("different archive")));
    assert.equal(result.status, 1);
    assert.match(result.stderr, /checksum mismatch/);
    assert.equal(fs.existsSync(binary), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("installer entrypoint exits non-zero when extraction fails", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "hallouminate-installer-"));
  try {
    const buffer = Buffer.from("not an archive");
    const { result, binary } = runInstaller(root, buffer, sha256(buffer));
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Install manually/);
    assert.equal(fs.existsSync(binary), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("launcher re-raises the signal that terminated the child", { skip: process.platform === "win32" }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "hallouminate-launcher-"));
  try {
    const binary = artifactFor(process.platform, process.arch).binary;
    fs.mkdirSync(path.join(root, "bin"));
    fs.copyFileSync(process.execPath, path.join(root, "bin", binary));
    fs.chmodSync(path.join(root, "bin", binary), 0o755);
    fs.copyFileSync(path.join(__dirname, "../run.js"), path.join(root, "run.js"));
    const result = spawnSync(process.execPath, [path.join(root, "run.js"), "-e", "process.kill(process.pid, 'SIGTERM')"]);
    assert.equal(result.signal, "SIGTERM");
    assert.equal(result.status, null);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("AC-13 package version and release runners", () => {
  const pkg = JSON.parse(fs.readFileSync(path.join(__dirname, "../package.json"), "utf8"));
  const cargo = fs.readFileSync(path.join(__dirname, "../../Cargo.toml"), "utf8");
  const workspaceVersion = cargo.match(/^version = "([^"]+)"/m)[1];
  assert.equal(pkg.version, workspaceVersion);
  const dist = fs.readFileSync(path.join(__dirname, "../../dist-workspace.toml"), "utf8");
  assert.match(dist, /^aarch64-apple-darwin = "macos-latest"$/m);
  assert.match(dist, /^x86_64-pc-windows-msvc = "windows-2022"$/m);
});

test("AC-20 Windows CI runners and timeout", () => {
  // Windows checkouts may convert ci.yml to CRLF; the job regex expects LF.
  const ci = fs.readFileSync(path.join(__dirname, "../../.github/workflows/ci.yml"), "utf8").replace(/\r\n/g, "\n");
  const jobBody = (name) => ci.match(new RegExp(`^  ${name}:\\n((?:    .*\\n|\\n)+)`, "m"))[1];
  const windowsJob = jobBody("windows");
  assert.match(windowsJob, /^    runs-on: windows-2022$/m);
  assert.match(windowsJob, /^    timeout-minutes: 60$/m);
  assert.match(jobBody("npm-installer"), /os: \[[^\]]*windows-2022[^\]]*\]/);
  assert.match(jobBody("verification-lease"), /os: \[[^\]]*windows-2022[^\]]*\]/);
  assert.equal(ci.includes("windows-latest"), false);
});

test("AC-20 required build gate depends on every gated job", () => {
  const ci = fs.readFileSync(path.join(__dirname, "../../.github/workflows/ci.yml"), "utf8");
  const gate = ci.match(/^  build-test:\n((?:    .*\n|\n)+)/m)[1];
  const needs = gate.match(/^    needs: \[([^\]]*)\]$/m)[1].split(",").map((name) => name.trim()).sort();
  assert.deepEqual(needs, ["build-test-matrix", "npm-installer", "verification-lease", "windows"]);
  assert.match(gate, /^    if: always\(\)$/m);
  assert.match(gate, /contains\(needs\.\*\.result, 'failure'\)/);
  assert.match(gate, /contains\(needs\.\*\.result, 'cancelled'\)/);
  assert.match(gate, /contains\(needs\.\*\.result, 'skipped'\)/);
});
