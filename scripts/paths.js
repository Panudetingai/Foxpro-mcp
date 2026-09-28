"use strict";

const path = require("path");

const PACKAGE_ROOT = path.join(__dirname, "..");
const VENDOR_DIR = path.join(PACKAGE_ROOT, "vendor");
const INSTALLED_MARKER = path.join(VENDOR_DIR, ".installed.json");

function getVendorBinaryPath(fileName) {
  return path.join(VENDOR_DIR, fileName);
}

function getLocalDevBinaryPath() {
  const name =
    process.platform === "win32" ? "foxpro-mcp.exe" : "foxpro-mcp";
  return path.join(PACKAGE_ROOT, "target", "release", name);
}

module.exports = {
  PACKAGE_ROOT,
  VENDOR_DIR,
  INSTALLED_MARKER,
  getVendorBinaryPath,
  getLocalDevBinaryPath,
};
