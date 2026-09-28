#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");
const { getBinaryFileName, isSupportedPlatform } = require("../scripts/platform");
const {
  getVendorBinaryPath,
  getLocalDevBinaryPath,
  INSTALLED_MARKER,
} = require("../scripts/paths");

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

if (!isSupportedPlatform()) {
  console.error(
    "foxpro-mcp supports Windows only (Visual FoxPro 9). Current platform is not supported.",
  );
  process.exit(1);
}

const binary = resolveBinary();
if (!binary) {
  printHelp();
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: false,
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

process.exit(result.status ?? 1);
