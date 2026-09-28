//! UI verification: launch VFP (or a built EXE) with a form/program, capture
//! screenshots of its window, and close it again.
//!
//! Screenshots use PowerShell + System.Drawing (`PrintWindow`) and therefore
//! only work on Windows; other platforms get an `UNSUPPORTED` error.

use crate::error::{FoxProError, Result};
use crate::fsutil;
use crate::vfp::{self, RuntimeError, VfpEngine};
use base64::Engine as _;
use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const MAX_INSTANCES: usize = 4;
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);
/// Images larger than this are saved but not inlined in the response.
pub const MAX_INLINE_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct InstanceInfo {
    pub instance_id: u32,
    pub pid: Option<u32>,
    pub target: String,
    pub started: DateTime<Local>,
    pub running: bool,
}

struct Instance {
    info: InstanceInfo,
    child: Child,
    run_dir: Option<PathBuf>,
}

impl Instance {
    fn runtime_errors(&self) -> Vec<RuntimeError> {
        self.run_dir
            .as_ref()
            .map(|d| vfp::parse_runtime_errors(&d.join("error.txt")))
            .unwrap_or_default()
    }
}

#[derive(Default)]
pub struct Instances {
    inner: Mutex<HashMap<u32, Instance>>,
    next: AtomicU32,
}

pub enum LaunchTarget {
    Form(PathBuf),
    Program(PathBuf),
    Executable(PathBuf),
    Project(PathBuf),
}

impl LaunchTarget {
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        Ok(match ext.as_str() {
            "scx" => LaunchTarget::Form(path.to_path_buf()),
            "prg" | "fxp" | "app" => LaunchTarget::Program(path.to_path_buf()),
            "exe" => LaunchTarget::Executable(path.to_path_buf()),
            "pjx" => LaunchTarget::Project(path.to_path_buf()),
            _ => {
                return Err(FoxProError::InvalidArgument(format!(
                    "cannot launch {}: expected .scx, .prg, .app, .exe or .pjx",
                    path.display()
                )));
            }
        })
    }

    fn path(&self) -> &Path {
        match self {
            LaunchTarget::Form(p)
            | LaunchTarget::Program(p)
            | LaunchTarget::Executable(p)
            | LaunchTarget::Project(p) => p,
        }
    }

    fn body(&self) -> String {
        match self {
            LaunchTarget::Form(p) => vfp::do_command(p),
            LaunchTarget::Program(p) => format!("DO {}", vfp::quote(p)),
            LaunchTarget::Project(p) => format!("MODIFY PROJECT {} NOWAIT", vfp::quote(p)),
            LaunchTarget::Executable(_) => String::new(),
        }
    }
}

impl Instances {
    /// Forget instances whose process has exited.
    fn reap(map: &mut HashMap<u32, Instance>) {
        for inst in map.values_mut() {
            if let Ok(Some(_)) = inst.child.try_wait() {
                inst.info.running = false;
            }
        }
    }

    pub async fn list(&self) -> Vec<InstanceInfo> {
        let mut map = self.inner.lock().await;
        Self::reap(&mut map);
        let mut v: Vec<InstanceInfo> = map.values().map(|i| i.info.clone()).collect();
        v.sort_by_key(|i| i.instance_id);
        v
    }

    pub async fn launch(
        &self,
        engine: Option<&VfpEngine>,
        target: LaunchTarget,
        dry_run: bool,
    ) -> Result<Value> {
        let target_display = target.path().to_string_lossy().into_owned();
        if dry_run {
            let command = match &target {
                LaunchTarget::Executable(p) => p.to_string_lossy().into_owned(),
                other => format!(
                    "{} -T <run-dir>/runner.prg  (runner: {})",
                    engine
                        .map(|e| e.executable().to_string_lossy().into_owned())
                        .unwrap_or_else(|| "vfp9.exe".into()),
                    other.body()
                ),
            };
            return Ok(json!({ "dry_run": true, "target": target_display, "command": command }));
        }

        let mut map = self.inner.lock().await;
        Self::reap(&mut map);
        map.retain(|_, i| i.info.running);
        if map.len() >= MAX_INSTANCES {
            return Err(FoxProError::Conflict(format!(
                "{MAX_INSTANCES} instances are already running; close one with foxpro.close first"
            )));
        }

        let (mut cmd, run_dir) = match &target {
            LaunchTarget::Executable(p) => {
                let mut cmd = Command::new(p);
                cmd.current_dir(p.parent().unwrap_or(Path::new(".")))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                (cmd, None)
            }
            other => {
                let engine = engine.ok_or_else(|| {
                    FoxProError::VfpNotConfigured("Set --vfp-path or FOXPRO_PATH".into())
                })?;
                let (dir, mut cmd) = engine.prepare_interactive(&other.body())?;
                cmd.stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                (cmd, Some(dir.persist()))
            }
        };

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Some(d) = &run_dir {
                    let _ = std::fs::remove_dir_all(d);
                }
                return Err(FoxProError::VfpNotConfigured(format!(
                    "failed to start: {e}"
                )));
            }
        };
        let pid = child.id();

        // Give the process a moment; report immediate failures.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if let Ok(Some(status)) = child.try_wait() {
            let errors = run_dir
                .as_ref()
                .map(|d| vfp::parse_runtime_errors(&d.join("error.txt")))
                .unwrap_or_default();
            if let Some(d) = &run_dir {
                let _ = std::fs::remove_dir_all(d);
            }
            return Ok(json!({
                "success": false,
                "target": target_display,
                "exited_immediately": true,
                "exit_code": status.code(),
                "errors": errors,
            }));
        }

        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let info = InstanceInfo {
            instance_id: id,
            pid,
            target: target_display,
            started: Local::now(),
            running: true,
        };
        let inst = Instance {
            info: info.clone(),
            child,
            run_dir,
        };
        let errors = inst.runtime_errors();
        map.insert(id, inst);
        Ok(json!({
            "success": errors.is_empty(),
            "instance": info,
            "errors": errors,
            "next": "use foxpro.screenshot with this instance_id, then foxpro.close",
        }))
    }

    pub async fn pid_of(&self, id: u32) -> Result<u32> {
        let mut map = self.inner.lock().await;
        Self::reap(&mut map);
        let inst = map
            .get(&id)
            .ok_or_else(|| FoxProError::NotFound(format!("no instance {id}")))?;
        if !inst.info.running {
            return Err(FoxProError::Conflict(format!(
                "instance {id} has already exited"
            )));
        }
        inst.info
            .pid
            .ok_or_else(|| FoxProError::Internal("instance has no process id".into()))
    }

    pub async fn close(&self, id: Option<u32>, dry_run: bool) -> Result<Value> {
        let mut map = self.inner.lock().await;
        let ids: Vec<u32> = match id {
            Some(id) if map.contains_key(&id) => vec![id],
            Some(id) => return Err(FoxProError::NotFound(format!("no instance {id}"))),
            None => map.keys().copied().collect(),
        };
        if dry_run {
            return Ok(json!({ "dry_run": true, "would_close": ids }));
        }
        let mut closed = Vec::new();
        for id in ids {
            if let Some(mut inst) = map.remove(&id) {
                let errors = inst.runtime_errors();
                vfp::kill_tree(&mut inst.child).await;
                if let Some(d) = &inst.run_dir {
                    let _ = std::fs::remove_dir_all(d);
                }
                closed.push(json!({
                    "instance_id": id,
                    "target": inst.info.target,
                    "errors": errors,
                }));
            }
        }
        Ok(json!({ "closed": closed }))
    }
}

/// Region relative to the captured window (pixels).
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub left: i64,
    pub top: i64,
    pub width: i64,
    pub height: i64,
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn screenshot_script(output: &Path, pid: Option<u32>, region: Option<Region>) -> String {
    let crop = match region {
        Some(r) => format!(
            "$rect = New-Object System.Drawing.Rectangle({}, {}, {}, {})\n\
             $rect.Intersect((New-Object System.Drawing.Rectangle(0, 0, $bmp.Width, $bmp.Height)))\n\
             if ($rect.Width -le 0 -or $rect.Height -le 0) {{ throw 'region is outside the captured image' }}\n\
             $crop = $bmp.Clone($rect, $bmp.PixelFormat); $bmp.Dispose(); $bmp = $crop\n",
            r.left, r.top, r.width, r.height
        ),
        None => String::new(),
    };
    format!(
        r#"$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class McpWin {{
  [StructLayout(LayoutKind.Sequential)] public struct RECT {{ public int Left; public int Top; public int Right; public int Bottom; }}
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}}
'@
[McpWin]::SetProcessDPIAware() | Out-Null
$procId = {pid}
if ($procId -gt 0) {{
  $p = Get-Process -Id $procId
  $h = [IntPtr]::Zero
  for ($i = 0; $i -lt 50 -and $h -eq [IntPtr]::Zero; $i++) {{
    $p.Refresh(); $h = $p.MainWindowHandle
    if ($h -eq [IntPtr]::Zero) {{ Start-Sleep -Milliseconds 100 }}
  }}
  if ($h -eq [IntPtr]::Zero) {{ throw 'the process has no visible window' }}
  $r = New-Object McpWin+RECT
  [McpWin]::GetWindowRect($h, [ref]$r) | Out-Null
  $w = $r.Right - $r.Left; $hh = $r.Bottom - $r.Top
  if ($w -le 0 -or $hh -le 0) {{ throw 'the window is minimized or has no size' }}
  $bmp = New-Object System.Drawing.Bitmap($w, $hh)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc()
  $ok = [McpWin]::PrintWindow($h, $hdc, 2)
  $g.ReleaseHdc($hdc); $g.Dispose()
  if (-not $ok) {{
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size); $g.Dispose()
  }}
}} else {{
  $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bmp = New-Object System.Drawing.Bitmap($b.Width, $b.Height)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size); $g.Dispose()
}}
{crop}$bmp.Save({out}, [System.Drawing.Imaging.ImageFormat]::Png)
Write-Output ("{{0}}x{{1}}" -f $bmp.Width, $bmp.Height)
$bmp.Dispose()
"#,
        pid = pid.unwrap_or(0),
        crop = crop,
        out = ps_quote(&output.to_string_lossy()),
    )
}

/// Capture a PNG of the window of `pid` (or the whole screen) into `output`.
/// Returns the image size reported by PowerShell ("WxH").
pub async fn capture(output: &Path, pid: Option<u32>, region: Option<Region>) -> Result<String> {
    if !cfg!(windows) {
        return Err(FoxProError::Unsupported(
            "screenshots are only available on Windows (PowerShell + System.Drawing)".into(),
        ));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let script = screenshot_script(output, pid, region);
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);

    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        &encoded,
    ])
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let out = tokio::time::timeout(SCREENSHOT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| FoxProError::Timeout(SCREENSHOT_TIMEOUT.as_secs()))??;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("unknown error");
        return Err(FoxProError::Internal(format!("screenshot failed: {first}")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn default_screenshot_path(workspace: &Path) -> PathBuf {
    workspace
        .join(vfp::STATE_DIR)
        .join("screenshots")
        .join(format!(
            "screenshot-{}-{}.png",
            Local::now().format("%Y%m%d_%H%M%S"),
            fsutil::unique_suffix()
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_target_by_extension() {
        assert!(matches!(
            LaunchTarget::from_path(Path::new("a.SCX")).unwrap(),
            LaunchTarget::Form(_)
        ));
        assert!(matches!(
            LaunchTarget::from_path(Path::new("a.exe")).unwrap(),
            LaunchTarget::Executable(_)
        ));
        assert!(LaunchTarget::from_path(Path::new("a.txt")).is_err());
    }

    #[test]
    fn screenshot_script_quotes_output_path() {
        let s = screenshot_script(Path::new("C:\\it's\\shot.png"), Some(42), None);
        assert!(s.contains("'C:\\it''s\\shot.png'"));
        assert!(s.contains("$procId = 42"));
    }

    #[tokio::test]
    async fn close_unknown_instance_is_an_error() {
        let instances = Instances::default();
        assert!(instances.close(Some(7), false).await.is_err());
        assert_eq!(
            instances.close(None, false).await.unwrap()["closed"],
            json!([])
        );
    }
}
