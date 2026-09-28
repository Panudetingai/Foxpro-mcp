"use strict";

/** @returns {string | null} Rust target triple used in release asset names */
function getRustTargetTriple() {
  if (process.platform === "win32") {
    if (process.arch === "x64") return "x86_64-pc-windows-msvc";
    if (process.arch === "arm64") return "aarch64-pc-windows-msvc";
  }
  return null;
}

function getBinaryFileName() {
  const triple = getRustTargetTriple();
  if (!triple) return null;
  return `foxpro-mcp-${triple}.exe`;
}

function isSupportedPlatform() {
  return getRustTargetTriple() !== null;
}

module.exports = {
  getRustTargetTriple,
  getBinaryFileName,
  isSupportedPlatform,
};
