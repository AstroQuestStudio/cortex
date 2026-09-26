#!/usr/bin/env node
// `npx astroquest-cortex <args>` / `cortex <args>`: runs the native binary, downloading it
// on first use if the postinstall step was skipped (e.g. npm --ignore-scripts).
"use strict";

const fs = require("fs");
const { spawn } = require("child_process");
const { binaryPath, install } = require("../install.js");

async function main() {
  let bin = binaryPath();
  if (!fs.existsSync(bin)) {
    process.stderr.write("[astroquest-cortex] downloading the cortex binary…\n");
    bin = await install();
  }
  const child = spawn(bin, process.argv.slice(2), { stdio: "inherit", windowsHide: true });
  for (const sig of ["SIGINT", "SIGTERM"]) process.on(sig, () => child.kill(sig));
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    else process.exit(code === null ? 1 : code);
  });
  child.on("error", (e) => {
    process.stderr.write(`[astroquest-cortex] cannot start ${bin}: ${e.message}\n`);
    process.exit(1);
  });
}

main().catch((e) => {
  process.stderr.write(`[astroquest-cortex] ${e.message}\n`);
  process.exit(1);
});
