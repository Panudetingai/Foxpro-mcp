# Build a Standalone Visual FoxPro MCP Server

คุณคือ Senior Rust Engineer, MCP Engineer และผู้เชี่ยวชาญด้าน Legacy Microsoft Visual FoxPro/VFP 9

ให้สร้างโปรเจกต์ **Standalone FoxPro MCP Server** สำหรับให้ AI Agent เช่น Claude Code, Codex, OpenCode และ Mali Cowork สามารถทำงานกับโปรเจกต์ **Microsoft Visual FoxPro 9** ได้อย่างมีประสิทธิภาพ

โปรเจกต์นี้ต้อง **แยกออกจาก Mali Cowork โดยสมบูรณ์** และต้องสามารถ build เป็น `foxpro-mcp.exe` บน Windows ได้

---

# 1. เป้าหมายหลัก

สร้าง MCP Server ที่ทำให้ AI Agent สามารถ:

1. วิเคราะห์ FoxPro project ที่มีอยู่
2. อ่าน/ค้นหา/แก้ไข FoxPro source code
3. สร้างและแก้ไข Form
4. สร้างและแก้ไข UI controls
5. สร้างและแก้ไข Class
6. สร้างและแก้ไข Report
7. อ่าน schema ของ DBF
8. Query DBF
9. Run FoxPro code
10. Build FoxPro project
11. ตรวจสอบ error จากการ build/run
12. ทำ iterative development loop:

* inspect
* modify
* build
* run
* inspect error
* fix
* build ใหม่

เป้าหมายคือให้ AI Agent สามารถพัฒนาและ maintain ระบบ FoxPro เดิมได้ ไม่ใช่แค่เป็น file editor

---

# 2. Technology Stack

ใช้:

* Rust stable
* Tokio
* serde
* serde_json
* tracing
* anyhow หรือ error type ที่เหมาะสม
* MCP SDK สำหรับ Rust ที่มีความเสถียร
* Windows-first architecture
* Visual FoxPro 9 เป็น runtime หลัก

ห้ามผูกกับ Mali Cowork

ห้ามใช้ Tauri

ห้ามสร้าง GUI ใน MCP Server

MCP Server ต้องเป็น headless process

สามารถทำงานผ่าน:

```text
stdio
```

เป็นหลัก

Executable:

```text
foxpro-mcp.exe
```

---

# 3. Project Structure

สร้างโครงสร้างประมาณนี้:

```text
foxpro-mcp/
│
├── Cargo.toml
├── README.md
├── LICENSE
├── .gitignore
│
├── src/
│   ├── main.rs
│   ├── server.rs
│   ├── config.rs
│   ├── error.rs
│   │
│   ├── tools/
│   │   ├── mod.rs
│   │   ├── project.rs
│   │   ├── code.rs
│   │   ├── form.rs
│   │   ├── class.rs
│   │   ├── report.rs
│   │   ├── database.rs
│   │   ├── build.rs
│   │   └── runtime.rs
│   │
│   ├── foxpro/
│   │   ├── mod.rs
│   │   ├── runner.rs
│   │   ├── project.rs
│   │   ├── scx.rs
│   │   ├── vcx.rs
│   │   ├── frx.rs
│   │   ├── dbf.rs
│   │   └── parser.rs
│   │
│   ├── security/
│   │   ├── mod.rs
│   │   ├── sandbox.rs
│   │   └── paths.rs
│   │
│   └── workspace/
│       ├── mod.rs
│       ├── index.rs
│       └── backup.rs
│
├── tests/
│   ├── project_tests.rs
│   ├── code_tests.rs
│   ├── form_tests.rs
│   ├── report_tests.rs
│   └── database_tests.rs
│
└── examples/
    └── sample-project/
```

สามารถปรับ structure ได้ถ้ามีเหตุผลทาง Rust architecture แต่ต้องรักษา separation ของ domain ให้ชัดเจน

---

# 4. MCP Tools

ต้อง implement tools ต่อไปนี้

## Project

### foxpro.open_project

เปิด FoxPro project:

```text
.pjx
```

Input:

```json
{
  "project_path": "D:\\ERP\\ERP.pjx"
}
```

ต้องตรวจสอบว่า path มีอยู่จริง

อ่าน project metadata

ค้นหา:

```text
.prg
.scx
.vcx
.frx
.dbf
.cdx
.fpt
.dbc
```

คืนข้อมูลโครงสร้าง project

---

### foxpro.inspect_project

วิเคราะห์ project โดยรวม:

```json
{
  "project_path": "D:\\ERP"
}
```

คืน:

```json
{
  "project": "ERP",
  "programs": [],
  "forms": [],
  "classes": [],
  "reports": [],
  "databases": []
}
```

---

### foxpro.search_code

ค้นหา code ใน project

Input:

```json
{
  "project_path": "D:\\ERP",
  "query": "SaveCustomer",
  "file_types": [".prg", ".scx", ".vcx"],
  "max_results": 100
}
```

คืน:

```json
{
  "results": [
    {
      "file": "customer.prg",
      "line": 42,
      "text": "PROCEDURE SaveCustomer"
    }
  ]
}
```

ต้องรองรับ case-insensitive search

---

# 5. Source Code Tools

## foxpro.read_code

อ่าน source code พร้อม line numbers

Input:

```json
{
  "file": "D:\\ERP\\customer.prg",
  "start_line": 1,
  "end_line": 200
}
```

---

## foxpro.write_code

แก้ source code

ต้องทำ:

1. ตรวจ path
2. ตรวจ workspace permission
3. backup file ก่อนแก้
4. write file
5. validate encoding
6. return diff summary

ห้าม overwrite โดยไม่มี backup

---

## foxpro.apply_patch

รองรับ patch แบบ targeted

ตัวอย่าง:

```json
{
  "file": "customer.prg",
  "old_text": "LOCAL lcName",
  "new_text": "LOCAL lcName, lcPhone"
}
```

ต้อง reject ถ้า old_text ไม่ตรงอย่างชัดเจน

ห้ามแก้ไฟล์แบบ fuzzy match ที่เสี่ยงแก้ผิดจุด

---

# 6. Form / UI System

นี่คือ feature สำคัญที่สุด

Visual FoxPro Form:

```text
.scx
```

ต้องสามารถ inspect และ manipulate ได้

## foxpro.inspect_form

Input:

```json
{
  "file": "D:\\ERP\\Forms\\Customer.scx"
}
```

แปลง SCX เป็น JSON representation ที่ AI เข้าใจง่าย

ตัวอย่าง:

```json
{
  "form": {
    "name": "CustomerForm",
    "width": 800,
    "height": 600
  },
  "controls": [
    {
      "name": "txtCustomerId",
      "type": "TextBox",
      "left": 100,
      "top": 50,
      "width": 200,
      "height": 24
    },
    {
      "name": "cmdSave",
      "type": "CommandButton",
      "caption": "บันทึก"
    }
  ]
}
```

---

# 7. Form Creation

Implement:

## foxpro.create_form

Input:

```json
{
  "project_path": "D:\\ERP",
  "file": "Forms\\Customer.scx",
  "name": "CustomerForm",
  "width": 800,
  "height": 600
}
```

ต้องสร้าง valid FoxPro SCX structure

---

# 8. Controls

Implement:

## foxpro.add_control

รองรับอย่างน้อย:

```text
Label
TextBox
EditBox
CommandButton
CheckBox
OptionButton
OptionGroup
ComboBox
ListBox
Grid
Image
Shape
Line
Container
PageFrame
Page
Timer
```

Input:

```json
{
  "form": "Forms\\Customer.scx",
  "type": "TextBox",
  "name": "txtCustomerName",
  "properties": {
    "Left": 150,
    "Top": 100,
    "Width": 250,
    "Height": 24,
    "Value": ""
  }
}
```

---

## foxpro.update_control

สามารถแก้:

```text
position
size
caption
value
font
color
visible
enabled
readonly
controlsource
rowsource
format
inputmask
```

และ property ที่ VFP รองรับ

---

## foxpro.remove_control

ลบ control ตาม name

ต้องตรวจสอบก่อนว่า control มีจริง

---

# 9. Event / Method System

Agent ต้องสามารถเพิ่ม event code เช่น:

```text
Click
Init
Destroy
Load
Unload
InteractiveChange
Valid
When
GotFocus
LostFocus
KeyPress
MouseDown
MouseUp
```

Tool:

```text
foxpro.update_method
```

ตัวอย่าง:

```json
{
  "form": "Forms\\Customer.scx",
  "object": "cmdSave",
  "method": "Click",
  "code": "MESSAGEBOX('Saved')"
}
```

ต้อง preserve existing methods

ห้าม overwrite method อื่นโดยไม่ตั้งใจ

---

# 10. Class / VCX

รองรับ:

```text
.vcx
.vct
```

Tools:

```text
foxpro.inspect_class
foxpro.create_class
foxpro.add_class_method
foxpro.update_class_method
foxpro.add_class_property
```

ตัวอย่าง:

```json
{
  "class": "CustomerService",
  "base_class": "Custom",
  "methods": [
    "Save",
    "Delete",
    "Find"
  ]
}
```

---

# 11. Report System

รองรับ:

```text
.frx
.frt
```

Tools:

```text
foxpro.inspect_report
foxpro.create_report
foxpro.add_report_field
foxpro.update_report_field
foxpro.remove_report_field
```

สามารถกำหนด:

```text
page size
orientation
margins
header
detail
footer
field
label
expression
group
group header
group footer
summary
```

ตัวอย่าง:

```json
{
  "report": "DailySales",
  "orientation": "portrait",
  "fields": [
    {
      "expression": "invoice_no",
      "caption": "เลขที่บิล",
      "x": 20,
      "y": 50,
      "width": 100
    }
  ]
}
```

---

# 12. Database / DBF

รองรับ:

```text
.dbf
.cdx
.fpt
```

Tools:

```text
foxpro.inspect_table
foxpro.describe_table
foxpro.sample_rows
foxpro.query_table
foxpro.find_records
```

ตัวอย่าง:

```json
{
  "table": "customer.dbf"
}
```

คืน:

```json
{
  "fields": [
    {
      "name": "customer_id",
      "type": "C",
      "length": 20
    },
    {
      "name": "name",
      "type": "C",
      "length": 100
    }
  ]
}
```

---

# 13. FoxPro Runtime

ต้องสามารถค้นหา VFP runtime ได้

รองรับ configuration:

```text
VFP9.EXE
VFP9SP2.EXE
```

เช่น:

```json
{
  "vfp_path": "C:\\Program Files\\Microsoft Visual FoxPro 9\\vfp9.exe"
}
```

หรือ environment variable:

```text
FOXPRO_PATH
```

---

# 14. foxpro.run

สามารถ execute FoxPro code หรือ PRG ได้

ตัวอย่าง:

```json
{
  "project_path": "D:\\ERP",
  "program": "test_customer.prg",
  "timeout_ms": 30000
}
```

ต้องคืน:

```json
{
  "success": true,
  "exit_code": 0,
  "stdout": "",
  "stderr": "",
  "duration_ms": 1234
}
```

---

# 15. Build

Implement:

```text
foxpro.build
```

สามารถ build:

```text
.pjx
```

ต้องคืน:

```json
{
  "success": false,
  "errors": [
    {
      "file": "customer.prg",
      "line": 183,
      "message": "..."
    }
  ]
}
```

ต้องพยายาม parse error ให้เป็น structured data

---

# 16. Test Loop

สร้าง tool:

```text
foxpro.test
```

Flow:

```text
build
 ↓
run
 ↓
capture error
 ↓
return structured result
```

Agent ต้องสามารถนำผลลัพธ์ไปแก้ code ต่อได้

---

# 17. Screenshot / UI Verification

ถ้าสามารถทำได้บน Windows ให้รองรับ:

```text
foxpro.launch
foxpro.screenshot
foxpro.close
```

Flow:

```text
Build
 ↓
Launch VFP
 ↓
Run Form
 ↓
Capture screenshot
 ↓
Agent วิเคราะห์ UI
 ↓
Modify Form
 ↓
Run ใหม่
```

หาก implementation screenshot มีความซับซ้อน ให้ทำเป็น Phase 2 แต่ต้องออกแบบ interface ไว้ตั้งแต่แรก

---

# 18. Workspace Security

MCP นี้จะถูกใช้โดย AI Agent ดังนั้นต้องมี workspace restriction

ตัวอย่าง:

```text
D:\ERP
```

Agent สามารถ:

```text
D:\ERP\*
```

แต่ห้าม:

```text
C:\Users\<user>\.ssh
C:\Users\<user>\.aws
C:\Windows
C:\Program Files
```

โดย default

ห้ามให้ MCP อ่าน/เขียน path นอก workspace

ต้องป้องกัน:

```text
..
symbolic links
junctions
absolute path escape
UNC path
```

ทุก filesystem operation ต้องผ่าน path validator กลาง

---

# 19. Backup

ก่อนแก้:

```text
customer.prg
```

สร้าง backup:

```text
.mcp-backup/
└── customer.prg.timestamp.bak
```

รองรับ rollback:

```text
foxpro.rollback
```

---

# 20. Dry Run

ทุก destructive operation ควรมี:

```json
{
  "dry_run": true
}
```

เช่น:

```text
add_control
update_control
remove_control
write_code
build
```

เมื่อ dry_run:

```text
ไม่แก้ไฟล์
ไม่ build
ไม่ run
```

แต่คืน preview/diff

---

# 21. Validation

ก่อน save SCX/VCX/FRX:

ตรวจ:

* required fields
* duplicate control names
* invalid properties
* invalid coordinates
* invalid class references
* malformed records
* corrupted FoxPro table structure

ห้ามสร้างไฟล์ที่ malformed ถ้าสามารถตรวจพบได้

---

# 22. Encoding

FoxPro legacy project อาจใช้ encoding ต่างกัน

ต้องออกแบบ encoding detection

รองรับอย่างน้อย:

```text
UTF-8
Windows-1252
Windows-874 / Thai
```

ห้ามแปลง encoding โดยไม่จำเป็น

ต้อง preserve original encoding เมื่อแก้ไฟล์

---

# 23. Project Index

สร้าง project index เพื่อให้ Agent ไม่ต้อง scan ทุกไฟล์ทุกครั้ง

ตัวอย่าง:

```text
.mcp/
└── index.json
```

เก็บ:

```text
files
forms
classes
methods
reports
tables
symbols
references
```

เมื่อไฟล์เปลี่ยน ให้ invalidate/re-index เฉพาะไฟล์ที่เปลี่ยน

---

# 24. Agent-Friendly Responses

MCP response ต้องไม่ส่งข้อมูลที่ใหญ่เกินความจำเป็น

ตัวอย่าง:

ไม่ควรส่ง source code 10,000 lines หาก Agent ขอหา function เดียว

ควรตอบ:

```json
{
  "file": "customer.prg",
  "symbol": "SaveCustomer",
  "line_start": 120,
  "line_end": 165
}
```

และให้ Agent request ต่อด้วย `read_code`

---

# 25. Error Handling

ทุก tool ต้องคืน structured error

ตัวอย่าง:

```json
{
  "success": false,
  "error": {
    "code": "FILE_NOT_FOUND",
    "message": "customer.prg was not found",
    "path": "D:\\ERP\\customer.prg"
  }
}
```

ห้าม panic จาก user input

ห้าม `unwrap()` ใน production path ที่อาจทำให้ MCP process crash

ใช้ Rust Result อย่างเหมาะสม

---

# 26. Logging

ใช้:

```text
tracing
```

log ไป stderr

ห้ามเขียน log ไป stdout เพราะ stdout ถูกใช้โดย MCP stdio protocol

รองรับ:

```text
RUST_LOG=info
RUST_LOG=debug
```

---

# 27. Configuration

รองรับ:

```text
FOXPRO_PATH
FOXPRO_WORKSPACE
FOXPRO_TIMEOUT
FOXPRO_LOG_LEVEL
```

และ config file:

```text
foxpro-mcp.json
```

ตัวอย่าง:

```json
{
  "foxpro_path": "C:\\Program Files\\Microsoft Visual FoxPro 9\\vfp9.exe",
  "workspace": "D:\\ERP",
  "timeout_ms": 30000,
  "backup_enabled": true
}
```

---

# 28. MCP Compatibility

MCP server ต้องสามารถใช้งานกับ client ทั่วไปได้

ทดสอบอย่างน้อย:

```text
Claude Code
Codex
OpenCode
Mali Cowork
```

อย่าทำ integration แบบ hard-code กับ client ใด client หนึ่ง

---

# 29. CLI

สร้าง command:

```text
foxpro-mcp.exe
```

รองรับ:

```text
foxpro-mcp.exe
foxpro-mcp.exe --config foxpro-mcp.json
foxpro-mcp.exe --workspace D:\ERP
foxpro-mcp.exe --help
foxpro-mcp.exe --version
```

MCP mode ใช้ stdio

---

# 30. Tests

ต้องมี automated tests สำหรับ:

* path validation
* workspace sandbox
* source parsing
* code search
* backup
* rollback
* form schema
* control creation
* control modification
* duplicate control detection
* report schema
* DBF inspection
* project indexing

และ integration tests ที่สามารถรันได้เมื่อมี VFP9 ติดตั้งอยู่

ห้ามทำ fake test อย่างเดียว

---

# 31. Important FoxPro Compatibility Rule

อย่าคิดว่า SCX/VCX/FRX เป็น JSON

SCX/VCX/FRX เป็น FoxPro table-based structures และต้องรักษา metadata/record structure ที่ Visual FoxPro ต้องการ

สร้าง abstraction layer:

```text
FoxPro Table
      ↓
Parser
      ↓
Domain Model
      ↓
JSON
      ↓
Agent
      ↓
Domain Model
      ↓
Serializer
      ↓
FoxPro Table
```

ห้ามให้ MCP logic ผูกกับ raw record manipulation กระจายไปทั่ว codebase

---

# 32. Domain Models

สร้าง Rust structs เช่น:

```rust
FormDefinition
ControlDefinition
MethodDefinition
ClassDefinition
ReportDefinition
ReportField
TableDefinition
FieldDefinition
ProjectDefinition
BuildResult
RuntimeResult
FoxProError
```

ใช้ serde สำหรับ serialization

---

# 33. AI UI Generation

ออกแบบ schema ให้ Agent สามารถสร้าง UI จาก natural language ได้

ตัวอย่าง:

User:

> สร้างฟอร์มค้นหาลูกค้า มีช่องรหัส ชื่อ และเบอร์โทร พร้อมปุ่มค้นหาและปุ่มล้างข้อมูล

Agent ควรสามารถ generate:

```json
{
  "form": {
    "name": "CustomerSearch",
    "width": 800,
    "height": 500
  },
  "controls": [
    {
      "type": "Label",
      "name": "lblId",
      "caption": "รหัสลูกค้า"
    },
    {
      "type": "TextBox",
      "name": "txtId"
    },
    {
      "type": "CommandButton",
      "name": "btnSearch",
      "caption": "ค้นหา"
    },
    {
      "type": "CommandButton",
      "name": "btnClear",
      "caption": "ล้างข้อมูล"
    }
  ]
}
```

จากนั้น MCP ต้องสร้าง valid FoxPro Form

---

# 34. Design Principles

ยึดหลัก:

```text
Safe
Deterministic
Reversible
Inspectable
Agent-friendly
Windows-first
FoxPro-compatible
```

ทุก mutation ต้อง:

```text
Validate
→ Backup
→ Modify
→ Validate
→ Return diff
```

---

# 35. Development Phases

อย่าพยายามทำทุก feature ในครั้งเดียว

## Phase 1 — Core MCP

ทำให้เสร็จก่อน:

```text
stdio MCP
workspace
read_code
write_code
search_code
open_project
inspect_project
run
build
errors
backup
```

ต้อง compile และทดสอบได้ก่อน

---

## Phase 2 — Form

เพิ่ม:

```text
inspect_form
create_form
add_control
update_control
remove_control
update_method
```

ต้องสามารถสร้าง:

```text
SCX
```

แล้วเปิดใน VFP ได้จริง

---

## Phase 3 — Report

เพิ่ม:

```text
inspect_report
create_report
add_report_field
update_report_field
remove_report_field
```

---

## Phase 4 — Database

เพิ่ม:

```text
inspect_table
describe_table
query_table
find_records
```

---

## Phase 5 — Agent Loop

เพิ่ม:

```text
build
run
error detection
screenshot
verification
```

ให้ Agent สามารถทำ:

```text
Understand
 ↓
Plan
 ↓
Modify
 ↓
Build
 ↓
Run
 ↓
Verify
 ↓
Fix
```

---

# 36. Do Not Overengineer

อย่าสร้าง:

* Web server
* Database server
* GUI
* cloud service
* authentication system
* account system
* telemetry

ใน MVP

ต้องเป็น:

```text
foxpro-mcp.exe
```

ตัวเดียวที่ติดตั้งบน Windows แล้วใช้งานได้

---

# 37. README

สร้าง README ที่อธิบาย:

1. What is FoxPro MCP
2. Requirements
3. Install
4. Configure VFP9 path
5. Configure workspace
6. Claude Code configuration
7. Codex configuration
8. OpenCode configuration
9. Mali Cowork configuration
10. Available tools
11. Security
12. Examples
13. Troubleshooting
14. Development
15. Build release

---

# 38. Final Acceptance Criteria

ถือว่า MVP สำเร็จเมื่อ:

### Code

Agent สามารถ:

```text
เปิด project
ค้นหา function
อ่าน source
แก้ source
backup
build
อ่าน error
แก้ error
build ใหม่
```

### Form

Agent สามารถ:

```text
สร้าง Form
เพิ่ม Label
เพิ่ม TextBox
เพิ่ม Button
แก้ property
เพิ่ม Click event
บันทึก SCX
เปิดด้วย VFP ได้
```

### Report

Agent สามารถ:

```text
สร้าง Report
เพิ่ม field
กำหนด layout
บันทึก FRX
เปิดด้วย VFP ได้
```

### Security

Agent ไม่สามารถ:

```text
อ่านไฟล์นอก workspace
เขียนไฟล์นอก workspace
path traversal
เข้าถึง arbitrary system files
```

### MCP

สามารถเชื่อมต่อผ่าน stdio ได้โดยไม่ต้องมี Mali Cowork

---

# 39. Implementation Instructions

เริ่มจาก:

```text
1. สร้าง Rust project
2. เพิ่ม MCP dependency ที่เหมาะสม
3. Implement stdio server
4. Implement error system
5. Implement workspace security
6. Implement project/code tools
7. เพิ่ม tests
8. Build บน Windows
9. จากนั้นจึงเริ่ม Form subsystem
```

อย่าเริ่มด้วย SCX ก่อนที่จะทำ MCP core และ workspace security เสร็จ

หลังจากแต่ละ phase:

```text
cargo check
cargo test
cargo clippy
cargo fmt --check
cargo build --release
```

ต้องแก้ errors/warnings ที่สำคัญก่อนดำเนิน phase ถัดไป

---

# 40. Expected Final Output

สุดท้ายต้องได้:

```text
foxpro-mcp/
└── target/
    └── release/
        └── foxpro-mcp.exe
```

และสามารถเรียก:

```text
foxpro-mcp.exe --workspace D:\MyFoxProProject
```

เพื่อเริ่ม MCP server

อย่าเพียงสร้าง skeleton หรือ TODO

ให้ implement functionality ที่ระบุจริงตามลำดับ Phase

เมื่อ feature ใดทำไม่ได้เพราะข้อจำกัดของ Visual FoxPro หรือ MCP SDK ให้ระบุข้อจำกัดนั้นอย่างชัดเจน และสร้าง abstraction/interface ที่เหมาะสมแทนการสร้าง implementation ที่หลอกว่าใช้งานได้

เริ่มลงมือสร้างโปรเจกต์ตั้งแต่ Phase 1 ทันที
