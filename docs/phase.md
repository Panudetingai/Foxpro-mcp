# Development Phases for FoxPro MCP Server

## Phase 1 – Core MCP
- Set up Rust project with Cargo
- Implement stdio‑based MCP server (request/response loop)
- Add configuration loading (environment variables & `foxpro-mcp.json`)
- Build path‑sandbox and workspace restriction module
- Provide CLI (`--workspace`, `--config`, `--help`, `--version`)
- Basic logging with `tracing`
- Implement error handling (`FoxProError`, `Result`)

## Phase 2 – Source Code Tools
- `foxpro.read_code` (line‑range, encoding detection)
- `foxpro.write_code` (backup before write, dry‑run support, diff output)
- `foxpro.search_code` (case‑insensitive, regex, file‑type filter)
- `foxpro.apply_patch` (exact‑match only, reject fuzzy)
- Backup system (`.mcp-backup/` with timestamps) and rollback tool

## Phase 3 – Build, Run & Test Loop
- Detect/configure Visual Fox Pro 9 runtime (`FOXPRO_PATH` or config)
- `foxpro.run` – execute `.prg` or inline code, timeout, capture stdout/stderr
- `foxpro.build` – invoke VFP project builder (or `msbuild` for `.vcx`/`.scx`), parse errors into structured JSON
- `foxpro.test` – orchestrate build → run → error capture, return structured result
- Implement dry‑run flags for mutating operations

## Phase 4 – Form / UI System
- Parse `.scx` (FoxPro form) into a JSON‑friendly `FormDefinition` model
- `foxpro.inspect_form` – return form metadata and controls
- `foxpro.create_form` – create a valid SCX file with given name/size
- `foxpro.add_control` – support at least Label, TextBox, EditBox, CommandButton, CheckBox, OptionButton, ComboBox, ListBox, Grid, Image, Shape, Line, Container, PageFrame, Page, Timer
- `foxpro.update_control` – modify position, size, caption, value, font, color, visibility, enabled, readonly, controlsource, rowsource, format, inputmask, etc.
- `foxpro.remove_control` – delete control by name (with existence check)
- `foxpro.update_method` – add/replace event/code (Click, Init, Destroy, etc.) while preserving other methods
- Validation: duplicate control names, required properties, valid coordinates, encoding preservation
- Backup before any modification

## Phase 5 – Report & Database
- Parse `.frx` (FoxPro report) into `ReportDefinition`
- `foxpro.inspect_report`, `foxpro.create_report`, `foxpro.add_report_field`, `foxpro.update_report_field`, `foxpro.remove_report_field`
- Support page size, orientation, margins, header/detail/footer, groups, expressions, labels
- Database layer:
  - `foxpro.inspect_table` – field list (name, type, length)
  - `foxpro.describe_table` – detailed schema (indexes, etc.)
  - `foxpro.query_table` – cursor‑based SELECT with `NEXT`, `SKIP`, `FIRST`, `RECNO`, `EOF()`
  - `foxpro.find_records` – locate records by expression
  - Support DBF, CDX, FPT files
  - Handle encodings: UTF‑8, Windows‑1252, Windows‑874 (Thai)
  - Use ODBC or direct DBF API abstraction; cursor abstraction yields rows as Rust structs/maps
- Validation before writing DBF/SCX/FRX files (field lengths, duplicate names, malformed records)

## Phase 6 – Agent Loop & Verification
- Provide high‑level `foxpro.agent_loop` (inspect → modify → build → run → verify → fix) as a convenience tool
- Optional UI verification:
  - `foxpro.launch` – start VFP with a given project/form
  - `foxpro.screenshot` – capture window or control screenshot
  - `foxpro.close` – terminate VFP instance
- Integrate with the backup/rollback system for safe iterative development
- Ensure all destructive operations have a `dry_run` mode that returns a preview/diff instead of mutating

## Phase 7 – Testing, CI & Release
- Write unit tests for every tool (property‑based and example based)
- Integration tests requiring a real VFP 9 installation (run on CI agents with VFP installed)
- Set up GitHub Actions (or similar) for `cargo test`, `cargo build --release`, and artifact upload
- Produce `foxpro-mcp.exe` (Windows release binary)
- Generate comprehensive `README.md` covering:
  - What is FoxPro MCP
  - Requirements (VFP 9, Rust toolchain)
  - Installation & configuration
  - Usage with Claude Code, Codex, OpenCode, Mali Cowork
  - Security model (workspace sandbox)
  - Troubleshooting guide
  - Development & contribution instructions

Each phase should be able to compile and pass its associated tests before moving to the next phase, ensuring a stable, incremental development process.
