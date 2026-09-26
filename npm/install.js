#!/usr/bin/env node
// Downloads the native `cortex` binary of this package's version from the GitHub
// release, checks its SHA-256 against SHA256SUMS.txt, and unpacks it in vendor/.
// No dependency: Node's https + crypto, and the system `tar` (Windows 10+, macOS, Linux).
//
// Environment:
//   CORTEX_BINARY         use this existing binary instead of downloading one
//   CORTEX_DOWNLOAD_BASE  mirror of https://github.com/AstroQuestStudio/cortex/releases/download
"use strict";

const fs = require("fs");
const path = require("path");
const https = require("https");
const crypto = require("crypto");
const { execFileSync } = require("child_process");

const pkg = require("./package.json");
const VENDOR = path.join(__dirname, "vendor");
const EXE = process.platform === "win32" ? "cortex.exe" : "cortex";
const BASE = process.env.CORTEX_DOWNLOAD_BASE || "https://github.com/AstroQuestStudio/cortex/releases/download";

const TARGETS = {
  "linux-x64": "x86_64-unknown-linux-gnu",
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
};

function binaryPath() {
  if (process.env.CORTEX_BINARY) return process.env.CORTEX_BINARY;
  return path.join(VENDOR, EXE);
}

function get(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": `astroquest-cortex-npm/${pkg.version}` } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location && redirects < 5) {
          res.resume();
          resolve(get(new URL(res.headers.location, url).toString(), redirects + 1));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`HTTP ${res.statusCode} for ${url}`));
          return;
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

// Unpacks an archive with the system tar. On Windows, the bsdtar of System32 reads .zip
// (a GNU tar found first on PATH, e.g. Git Bash's, does not); PowerShell is the fallback.
function extract(archive, dest) {
  if (process.platform === "win32") {
    const bsdtar = path.join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe");
    if (fs.existsSync(bsdtar)) {
      execFileSync(bsdtar, ["-xf", archive, "-C", dest], { stdio: "ignore" });
      return;
    }
    execFileSync(
      "powershell.exe",
      ["-NoProfile", "-Command", `Expand-Archive -LiteralPath '${archive}' -DestinationPath '${dest}' -Force`],
      { stdio: "ignore" },
    );
    return;
  }
  execFileSync("tar", ["-xzf", archive, "-C", dest], { stdio: "ignore" });
}

async function install() {
  const key = `${process.platform}-${process.arch}`;
  const target = TARGETS[key];
  if (!target) {
    throw new Error(`no prebuilt binary for ${key}; install from source: cargo install astroquest-cortex`);
  }
  const archive = `cortex-${target}${process.platform === "win32" ? ".zip" : ".tar.gz"}`;
  const base = `${BASE}/v${pkg.version}`;
  const [data, sums] = await Promise.all([get(`${base}/${archive}`), get(`${base}/SHA256SUMS.txt`)]);

  const expected = sums
    .toString("utf8")
    .split(/\r?\n/)
    .map((l) => l.trim().split(/\s+\*?/))
    .find((p) => p[1] === archive);
  if (!expected) throw new Error(`${archive} is missing from SHA256SUMS.txt`);
  const actual = crypto.createHash("sha256").update(data).digest("hex");
  if (actual !== expected[0].toLowerCase()) {
    throw new Error(`checksum mismatch for ${archive}: expected ${expected[0]}, got ${actual}`);
  }

  fs.mkdirSync(VENDOR, { recursive: true });
  const tmp = path.join(VENDOR, archive);
  fs.writeFileSync(tmp, data);
  try {
    extract(tmp, VENDOR);
  } finally {
    fs.rmSync(tmp, { force: true });
  }
  const bin = path.join(VENDOR, EXE);
  if (!fs.existsSync(bin)) throw new Error(`${EXE} not found in ${archive}`);
  if (process.platform !== "win32") fs.chmodSync(bin, 0o755);
  return bin;
}

module.exports = { binaryPath, install, extract };

if (require.main === module) {
  const postinstall = process.argv.includes("--postinstall");
  if (process.env.CORTEX_BINARY || fs.existsSync(binaryPath())) process.exit(0);
  install()
    .then((bin) => {
      if (!postinstall) console.log(`cortex installed: ${bin}`);
    })
    .catch((e) => {
      // Never fail `npm install`: bin/cortex.js retries on first run.
      console.error(`[astroquest-cortex] download failed (${e.message}); it will be retried on first run.`);
      process.exit(postinstall ? 0 : 1);
    });
}
