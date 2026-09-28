#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const fs = require("fs");
const { getBinaryFileName, isSupportedPlatform } = require("../scripts/platform");
const {
  getVendorBinaryPath,
  getLocalDevBinaryPath,
  INSTALLED_MARKER,
} = require("../scripts/paths");
const { ensureBinary } = require("../scripts/install");

function resolveBinary() {
  if (process.env.FOXPRO_MCP_BIN) {
    return process.env.FOXPRO_MCP_BIN;
  }

  const fileName = getBinaryFileName();
  if (fileName) {
    const vendor = getVendorBinaryPath(fileName);
    if (fs.existsSync(vendor)) return vendor;
  }

  const local = getLocalDevBinaryPath();
  if (fs.existsSync(local)) return local;

  return null;
}

function printHelp() {
  console.error(`foxpro-mcp: native binary not found.

Visual FoxPro MCP requires Windows (win32, x64 or arm64) and Visual FoxPro 9.

If you just installed via npm, re-run:
  npm rebuild foxpro-mcp

Or set FOXPRO_MCP_BIN to your foxpro-mcp.exe path.
Install marker: ${INSTALLED_MARKER}
`);
}

async function main() {
  if (!isSupportedPlatform()) {
    console.error(
      "foxpro-mcp supports Windows only (Visual FoxPro 9). Current platform is not supported.",
    );
    process.exit(1);
  }

  // postinstall may not have run (pnpm/bun block install scripts by default,
  // or `--ignore-scripts`), so download lazily on first launch.
  const binary = resolveBinary() ?? (await ensureBinary());
  if (!binary) {
    printHelp();
    process.exit(1);
  }

  const child = spawn(binary, process.argv.slice(2), {
    stdio: "inherit",
    windowsHide: false,
  });

  for (const sig of ["SIGINT", "SIGTERM", "SIGBREAK"]) {
    process.on(sig, () => child.kill(sig));
  }

  child.on("error", (err) => {
    console.error(err.message);
    process.exit(1);
  });
  child.on("exit", (code, signal) => {
    process.exit(code ?? (signal ? 1 : 0));
  });
}

main().catch((err) => {
  console.error(err.stack || String(err));
  process.exit(1);
});
