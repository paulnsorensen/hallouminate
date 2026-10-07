"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", windowsHide: true, ...options });
    child.once("error", reject);
    child.once("close", (code) => {
      if (code === 0) resolve();
      else reject(new Error(`${command} extraction failed (exit ${code})`));
    });
  });
}

async function installArchive(buffer, artifact, binDir, platform = process.platform) {
  const staging = fs.mkdtempSync(path.join(os.tmpdir(), "hallouminate-install-"));
  try {
    const archive = path.join(staging, artifact.archive);
    const extracted = path.join(staging, "extracted");
    fs.writeFileSync(archive, buffer);
    fs.mkdirSync(extracted);
    if (platform === "win32") {
      await run("powershell.exe", [
        "-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
        "$ErrorActionPreference = 'Stop'; Expand-Archive -LiteralPath $env:HALLOUMINATE_ARCHIVE -DestinationPath $env:HALLOUMINATE_EXTRACTED",
      ], { env: { ...process.env, HALLOUMINATE_ARCHIVE: archive, HALLOUMINATE_EXTRACTED: extracted } });
    } else {
      await run("tar", ["-xJf", archive, "--strip-components=1", "-C", extracted]);
    }
    const binary = path.join(extracted, artifact.binary);
    if (!fs.existsSync(binary) || !fs.statSync(binary).isFile()) {
      throw new Error(`binary missing after extraction: ${artifact.binary}`);
    }
    fs.mkdirSync(binDir, { recursive: true });
    for (const entry of fs.readdirSync(extracted)) {
      if (entry !== artifact.binary) {
        fs.cpSync(path.join(extracted, entry), path.join(binDir, entry), { recursive: true });
      }
    }
    if (platform !== "win32") fs.chmodSync(binary, 0o755);
    fs.copyFileSync(binary, path.join(binDir, artifact.binary));
  } finally {
    fs.rmSync(staging, { recursive: true, force: true });
  }
}

module.exports = { installArchive };
