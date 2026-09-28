# foxpro-mcp

MCP server สำหรับ **Visual FoxPro 9** — ให้ AI agent อ่าน/แก้โค้ด FoxPro, form, report, DBF และรัน/build ผ่าน VFP ได้

ผู้ใช้ติดตั้งผ่าน **npm** ได้เลย **ไม่ต้องติดตั้ง Rust หรือ Python**

## สิ่งที่ต้องมี

| สิ่งที่ต้องมี | หมายเหตุ |
| --- | --- |
| **Node.js 18+** | ใช้ติดตั้งและรันคำสั่ง `foxpro-mcp` |
| **Windows (x64 หรือ arm64)** | VFP 9 รันบน Windows |
| **Visual FoxPro 9** | สำหรับ run/build จริง (ตั้ง path ใน config) |

## ติดตั้ง

### ติดตั้งแบบ global

```bash
npm install -g foxpro-mcp
```

### ติดตั้งในโปรเจกต์

```bash
npm install foxpro-mcp
```

### ใช้ครั้งเดียว (ไม่ต้อง global)

```bash
npx foxpro-mcp --help
```

ตอน `npm install` (หรือตอนรันครั้งแรก) จะดาวน์โหลด `foxpro-mcp.exe` ที่ build แล้วจาก [GitHub Releases](https://github.com/Panudetingai/Foxpro-mcp/releases) อัตโนมัติ

## ตั้งค่า MCP (ตัวอย่าง)

สร้างไฟล์ `foxpro-mcp.json` ใน workspace:

```json
{
  "workspace": ".",
  "vfp_path": "C:/Program Files (x86)/Microsoft Visual FoxPro 9/vfp9.exe",
  "vfp_timeout": 60,
  "log_level": "info"
}
```

> ใช้ path แบบ absolute กับ `--config` เสมอ — MCP client แต่ละตัวเปิด server จาก working directory ต่างกัน

**แบบ `npx` (แนะนำ — ไม่ต้องติดตั้งก่อน)**

Claude Desktop / Cursor / Windsurf:

```json
{
  "mcpServers": {
    "foxpro": {
      "command": "npx",
      "args": ["-y", "foxpro-mcp@latest", "--config", "C:/path/to/your/project/foxpro-mcp.json"]
    }
  }
}
```

ถ้า client ขึ้น error ว่าหา `npx` ไม่เจอ (`spawn npx ENOENT`) บน Windows ให้ครอบด้วย `cmd /c`:

```json
{
  "mcpServers": {
    "foxpro": {
      "command": "cmd",
      "args": ["/c", "npx", "-y", "foxpro-mcp@latest", "--config", "C:/path/to/your/project/foxpro-mcp.json"]
    }
  }
}
```

Claude Code (Windows native ต้องใช้ `cmd /c` เสมอ):

```bash
claude mcp add foxpro -- cmd /c npx -y foxpro-mcp@latest --config C:/path/to/your/project/foxpro-mcp.json
```

ครั้งแรกที่รันจะดาวน์โหลด `foxpro-mcp.exe` (~6 MB) อาจใช้เวลาสักครู่ ครั้งต่อไปใช้จาก cache

**แบบติดตั้ง global** (`npm install -g foxpro-mcp`) — เริ่มเร็วกว่า ไม่ต้องรอ npx:

```json
{
  "mcpServers": {
    "foxpro": {
      "command": "foxpro-mcp",
      "args": ["--config", "C:/path/to/your/project/foxpro-mcp.json"]
    }
  }
}
```

> ใช้ pnpm / bun / `--ignore-scripts` ได้ — ถ้า postinstall ไม่ได้รัน `foxpro-mcp` จะดาวน์โหลด binary ให้เองตอนเปิดครั้งแรก

## CLI

```bash
foxpro-mcp --help
foxpro-mcp --workspace C:/path/to/vfp-project --vfp-path "C:/Program Files (x86)/Microsoft Visual FoxPro 9/vfp9.exe"
```

## แก้ปัญหา

**Binary ไม่ถูกดาวน์โหลด**

```bash
npm rebuild foxpro-mcp
```

หรือชี้ไปที่ exe เอง:

```bash
set FOXPRO_MCP_BIN=C:\path\to\foxpro-mcp.exe
```

**พัฒนา Rust ใน repo นี้ (maintainer)**

```bash
cargo build --release
npm install
# postinstall จะใช้ target/release/foxpro-mcp.exe ถ้ายังไม่มี GitHub release
```

ข้ามการดาวน์โหลด:

```bash
set FOXPRO_MCP_SKIP_DOWNLOAD=1
set FOXPRO_MCP_BIN=target\release\foxpro-mcp.exe
```

## พัฒนา (Rust)

Maintainer เท่านั้นที่ต้องมี Rust toolchain:

```bash
cargo test
cargo build --release
```

Release binary สำหรับ npm ถูกสร้างจาก GitHub Actions เมื่อ push tag `v*` (ดู `.github/workflows/release.yml`)

### Publish ครั้งแรก (maintainer)

1. ตั้ง `NPM_TOKEN` ใน GitHub repository secrets (ถ้าต้องการให้ CI publish npm อัตโนมัติ)
2. อัปเดต `version` ใน `package.json` และ `Cargo.toml` ให้ตรงกัน
3. สร้างและ push tag:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

   CI จะ build `foxpro-mcp-x86_64-pc-windows-msvc.exe` (และ arm64) แนบใน GitHub Release — หลัง `npm install` ผู้ใช้จะดาวน์โหลดไฟล์เหล่านี้

## ความปลอดภัย

การเข้าถึงไฟล์ถูกจำกัดอยู่ใน `workspace` ที่กำหนดใน config — ดูรายละเอียดใน `docs/foxpro-mcp-spec.md`

## License

MIT
