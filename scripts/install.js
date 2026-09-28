"use strict";

const fs = require("fs");
const https = require("https");
const path = require("path");
const { getBinaryFileName, isSupportedPlatform } = require("./platform");
const {
  VENDOR_DIR,
  INSTALLED_MARKER,
  getVendorBinaryPath,
  getLocalDevBinaryPath,
  PACKAGE_ROOT,
} = require("./paths");

const REPO = "Panudetingai/Foxpro-mcp";
const pkg = require(path.join(PACKAGE_ROOT, "package.json"));

function log(msg) {
  // stdout is the MCP stdio channel — never write logs there.
  console.error(`[foxpro-mcp] ${msg}`);
}

function readMarker() {
  try {
    return JSON.parse(fs.readFileSync(INSTALLED_MARKER, "utf8"));
  } catch {
    return null;
  }
}

function writeMarker(data) {
  fs.mkdirSync(VENDOR_DIR, { recursive: true });
  fs.writeFileSync(INSTALLED_MARKER, JSON.stringify(data, null, 2));
}

function copyFile(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
}

function tryUseLocalDevBinary(destPath, fileName) {
  const local = getLocalDevBinaryPath();
  if (!fs.existsSync(local)) return false;
  log(`Using local release binary: ${local}`);
  copyFile(local, destPath);
  writeMarker({
    version: pkg.version,
    fileName,
    source: "local-dev",
    installedAt: new Date().toISOString(),
  });
  return true;
}

function downloadFile(url, destPath, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 10) {
      reject(new Error(`Too many redirects: ${url}`));
      return;
    }
    https
      .get(url, { headers: { "User-Agent": "foxpro-mcp-installer" } }, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode)) {
          res.resume();
          const loc = res.headers.location;
          if (!loc) {
            reject(new Error(`Redirect without location: ${url}`));
            return;
          }
          downloadFile(new URL(loc, url).toString(), destPath, redirects + 1).then(
            resolve,
            reject,
          );
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`HTTP ${res.statusCode} downloading ${url}`));
          return;
        }
        const file = fs.createWriteStream(destPath);
        res.pipe(file);
        file.on("finish", () => file.close(resolve));
        file.on("error", (err) => {
          fs.unlink(destPath, () => {});
          reject(err);
        });
        res.on("error", (err) => {
          file.close();
          fs.unlink(destPath, () => {});
          reject(err);
        });
      })
      .on("error", reject);
  });
}

async function downloadFromGitHubRelease(destPath, fileName) {
  const tag = `v${pkg.version}`;
  // Direct asset URL — avoids the unauthenticated GitHub API rate limit (60 req/h).
  const url = `https://github.com/${REPO}/releases/download/${tag}/${fileName}`;
  log(`Downloading ${fileName} from ${tag}…`);

  // Download to a temp file and rename, so an interrupted download never
  // leaves a truncated exe that the launcher would try to run.
  fs.mkdirSync(path.dirname(destPath), { recursive: true });
  const tmpPath = `${destPath}.${process.pid}.download`;
  try {
    await downloadFile(url, tmpPath);
    fs.renameSync(tmpPath, destPath);
  } catch (err) {
    fs.rmSync(tmpPath, { force: true });
    throw err;
  }

  writeMarker({
    version: pkg.version,
    fileName,
    source: "github-release",
    tag,
    installedAt: new Date().toISOString(),
  });
}

/**
 * Makes sure the native binary for this platform is in vendor/.
 * @returns {Promise<string | null>} path to the binary, or null if unavailable
 */
async function ensureBinary() {
  if (!isSupportedPlatform()) {
    log("Skipping binary download: foxpro-mcp runs on Windows only (win32 x64/arm64).");
    return null;
  }

  const fileName = getBinaryFileName();
  const destPath = getVendorBinaryPath(fileName);

  const marker = readMarker();
  if (
    marker?.version === pkg.version &&
    marker?.fileName === fileName &&
    fs.existsSync(destPath)
  ) {
    return destPath;
  }

  if (process.env.FOXPRO_MCP_BIN && fs.existsSync(process.env.FOXPRO_MCP_BIN)) {
    log(`Using FOXPRO_MCP_BIN=${process.env.FOXPRO_MCP_BIN}`);
    copyFile(process.env.FOXPRO_MCP_BIN, destPath);
    writeMarker({
      version: pkg.version,
      fileName,
      source: "env",
      installedAt: new Date().toISOString(),
    });
    return destPath;
  }

  try {
    await downloadFromGitHubRelease(destPath, fileName);
    log("Install complete.");
    return destPath;
  } catch (err) {
    if (tryUseLocalDevBinary(destPath, fileName)) {
      log("Install complete (local dev binary).");
      return destPath;
    }
    log(`Could not download binary: ${err.message}`);
    log(
      "Build locally with `cargo build --release`, set FOXPRO_MCP_BIN, or publish GitHub release assets.",
    );
    return null;
  }
}

async function main() {
  if (process.env.FOXPRO_MCP_SKIP_DOWNLOAD === "1") {
    log("FOXPRO_MCP_SKIP_DOWNLOAD=1 — skipping binary download.");
    return;
  }
  const binary = await ensureBinary();
  if (!binary) {
    log("The binary will be downloaded again the first time `foxpro-mcp` runs.");
  }
}

module.exports = { ensureBinary };

if (require.main === module) {
  // Never fail `npm install` — the launcher retries the download on first run.
  main().catch((err) => {
    log(err.stack || String(err));
    process.exit(0);
  });
}
