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

function fetchJson(url) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": "foxpro-mcp-installer" } }, (res) => {
        if (res.statusCode === 302 || res.statusCode === 301) {
          const loc = res.headers.location;
          if (!loc) {
            reject(new Error(`Redirect without location: ${url}`));
            return;
          }
          fetchJson(loc).then(resolve, reject);
          return;
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => {
          const body = Buffer.concat(chunks).toString("utf8");
          if (res.statusCode !== 200) {
            reject(new Error(`HTTP ${res.statusCode} for ${url}: ${body.slice(0, 200)}`));
            return;
          }
          try {
            resolve(JSON.parse(body));
          } catch (e) {
            reject(e);
          }
        });
      })
      .on("error", reject);
  });
}

function downloadFile(url, destPath) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(destPath);
    https
      .get(url, { headers: { "User-Agent": "foxpro-mcp-installer" } }, (res) => {
        if (res.statusCode === 302 || res.statusCode === 301) {
          const loc = res.headers.location;
          file.close();
          fs.unlink(destPath, () => {});
          if (!loc) {
            reject(new Error(`Redirect without location: ${url}`));
            return;
          }
          downloadFile(loc, destPath).then(resolve, reject);
          return;
        }
        if (res.statusCode !== 200) {
          file.close();
          fs.unlink(destPath, () => {});
          reject(new Error(`HTTP ${res.statusCode} downloading ${url}`));
          return;
        }
        res.pipe(file);
        file.on("finish", () => file.close(resolve));
      })
      .on("error", (err) => {
        file.close();
        fs.unlink(destPath, () => {});
        reject(err);
      });
  });
}

async function downloadFromGitHubRelease(destPath, fileName) {
  const tag = `v${pkg.version}`;
  const apiUrl = `https://api.github.com/repos/${REPO}/releases/tags/${tag}`;
  const release = await fetchJson(apiUrl);
  const asset = release.assets?.find((a) => a.name === fileName);
  if (!asset) {
    throw new Error(
      `Release ${tag} has no asset "${fileName}". Publish a GitHub release first.`,
    );
  }
  log(`Downloading ${fileName} from ${tag}…`);
  await downloadFile(asset.browser_download_url, destPath);
  writeMarker({
    version: pkg.version,
    fileName,
    source: "github-release",
    tag,
    installedAt: new Date().toISOString(),
  });
}

async function main() {
  if (process.env.FOXPRO_MCP_SKIP_DOWNLOAD === "1") {
    log("FOXPRO_MCP_SKIP_DOWNLOAD=1 — skipping binary download.");
    return;
  }

  if (!isSupportedPlatform()) {
    log(
      "Skipping binary download: foxpro-mcp runs on Windows only (win32 x64/arm64).",
    );
    return;
  }

  const fileName = getBinaryFileName();
  const destPath = getVendorBinaryPath(fileName);

  const marker = readMarker();
  if (
    marker?.version === pkg.version &&
    marker?.fileName === fileName &&
    fs.existsSync(destPath)
  ) {
    log(`Binary already installed (${fileName}).`);
    return;
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
    return;
  }

  try {
    await downloadFromGitHubRelease(destPath, fileName);
    log("Install complete.");
  } catch (err) {
    if (tryUseLocalDevBinary(destPath, fileName)) {
      log("Install complete (local dev binary).");
      return;
    }
    log(`Could not download binary: ${err.message}`);
    log(
      "Build locally with `cargo build --release`, set FOXPRO_MCP_BIN, or publish GitHub release assets.",
    );
    log("The `foxpro-mcp` command will not work until a binary is available.");
  }
}

main().catch((err) => {
  log(err.stack || String(err));
  process.exit(0);
});
