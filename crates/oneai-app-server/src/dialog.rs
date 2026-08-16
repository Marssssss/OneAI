//! Native OS directory picker — the deepseek-harness parity affordance.
//!
//! The web frontend can't get a host absolute path from any browser picker
//! (security sandbox), so the local sidecar shows the platform's native
//! folder-chooser and returns the chosen path over a JSON-RPC call
//! (`dialog/pick_directory`). Mirrors deepseek-harness's
//! `directory-picker-native`: `osascript choose folder` (macOS),
//! `zenity`/`kdialog` (Linux), PowerShell `FolderBrowserDialog` (Windows).
//! No file upload — the path is absolute and the agent operates on the real
//! folder.
//!
//! Cancel → `None`. Picker-missing / unexpected error → `None` + a warn log
//! (the frontend can fall back to a typed path).

#[cfg(target_os = "macos")]
const PROMPT: &str = "选择工作区目录";

/// Open the platform native directory picker; return the chosen absolute
/// path, or `None` on cancel / unavailable.
pub async fn pick_directory() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        pick_macos().await
    }
    #[cfg(target_os = "linux")]
    {
        pick_linux().await
    }
    #[cfg(target_os = "windows")]
    {
        pick_windows().await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
async fn pick_macos() -> Option<String> {
    // `choose folder` pops the native NSOpenPanel-style folder picker (the
    // "选取" button, path-in-title dialog the user sees). `POSIX path of`
    // prints the absolute path to stdout. Cancel = osascript exit 1 with
    // stderr -128 / "User canceled".
    let out = tokio::process::Command::new("osascript")
        .args([
            "-e",
            &format!("set selectedFolder to choose folder with prompt \"{PROMPT}\""),
            "-e",
            "POSIX path of selectedFolder",
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if path.is_empty() {
                None
            } else {
                Some(path)
            }
        }
        Ok(o) => {
            // Non-zero — cancel (exit 1, stderr -128) or a real error. Either
            // way the user gets no selection; surface a warn for non-cancel.
            let stderr = String::from_utf8_lossy(&o.stderr);
            let canceled = stderr.contains("-128") || stderr.contains("User canceled");
            if !canceled {
                tracing::warn!("osascript choose folder failed: {stderr}");
            }
            None
        }
        Err(e) => {
            tracing::warn!("osascript not available: {e}");
            None
        }
    }
}

#[cfg(target_os = "linux")]
async fn pick_linux() -> Option<String> {
    // zenity first (GNOME/most distros); fall back to kdialog (KDE). Both exit
    // 1 on cancel; ENOENT ⇒ not installed ⇒ try the next.
    if let Some(p) = run_picker(
        "zenity",
        &["--file-selection", "--directory", "--title=选择工作区目录"],
    )
    .await
    {
        return Some(p);
    }
    if let Some(p) = run_picker(
        "kdialog",
        &["--getexistingdirectory", ".", "--title", "选择工作区目录"],
    )
    .await
    {
        return Some(p);
    }
    tracing::warn!("no supported native directory picker (install zenity or kdialog)");
    None
}

#[cfg(target_os = "linux")]
async fn run_picker(bin: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(bin).args(args).output().await;
    match out {
        Ok(o) if o.status.success() => {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if p.is_empty() {
                None
            } else {
                Some(p)
            }
        }
        Ok(_) => None, // cancel (exit 1) or error → no selection
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None, // not installed
        Err(e) => {
            tracing::warn!("{bin} picker error: {e}");
            None
        }
    }
}

#[cfg(target_os = "windows")]
async fn pick_windows() -> Option<String> {
    // PowerShell FolderBrowserDialog under -STA. Best-effort; the harness uses
    // a native IFileOpenDialog child — refine to `rfd` later if this proves
    // flaky on some hosts.
    let ps = r#"$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.FolderBrowserDialog
$d.Description = '选择工作区目录'
if ($d.ShowDialog() -eq 'OK') { [System.Console]::Out.Write($d.SelectedPath) } else { [System.Console]::Out.Write('') }
"#;
    let out = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", ps])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if p.is_empty() {
                None
            } else {
                Some(p)
            }
        }
        Ok(o) => {
            tracing::warn!(
                "powershell folder dialog failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            None
        }
        Err(e) => {
            tracing::warn!("powershell not available: {e}");
            None
        }
    }
}
