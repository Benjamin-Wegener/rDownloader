//! Platform-specific autostart configuration (macOS LaunchAgent, Linux XDG desktop entry, Windows Registry).

#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::Context;
use anyhow::Result;
use std::path::PathBuf;

/// Path to the current executable.
fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rdownloader"))
}

/// Sets the autostart state for the current user.
pub fn set_autostart(enable: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        set_macos_launch_agent(enable)
    }
    #[cfg(target_os = "linux")]
    {
        set_linux_xdg_autostart(enable)
    }
    #[cfg(target_os = "windows")]
    {
        set_windows_registry_autostart(enable)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = enable;
        Ok(())
    }
}

/// Checks whether autostart is currently enabled for the current user.
pub fn is_autostart_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_launch_agent_path().is_some_and(|p| p.exists())
    }
    #[cfg(target_os = "linux")]
    {
        linux_xdg_autostart_path().is_some_and(|p| p.exists())
    }
    #[cfg(target_os = "windows")]
    {
        check_windows_registry_autostart()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

// ───────────────────────── macOS LaunchAgent ─────────────────────────

#[cfg(target_os = "macos")]
pub fn macos_launch_agent_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library")
            .join("LaunchAgents")
            .join("org.rdownloader.app.plist")
    })
}

#[cfg(target_os = "macos")]
fn set_macos_launch_agent(enable: bool) -> Result<()> {
    let path = macos_launch_agent_path().context("could not locate user home directory")?;
    if enable {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let exe = current_exe();
        let exe_str = exe.to_string_lossy();
        let plist_content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>org.rdownloader.app</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_str}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
        );
        std::fs::write(&path, plist_content)?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ───────────────────────── Linux XDG Autostart ─────────────────────────

#[cfg(target_os = "linux")]
pub fn linux_xdg_autostart_path() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("autostart").join("rdownloader.desktop"))
}

#[cfg(target_os = "linux")]
fn set_linux_xdg_autostart(enable: bool) -> Result<()> {
    let path = linux_xdg_autostart_path().context("could not locate XDG config directory")?;
    if enable {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let exe = current_exe();
        let exe_str = exe.to_string_lossy();
        let desktop_entry = format!(
            r#"[Desktop Entry]
Type=Application
Version=1.0
Name=rDownloader
GenericName=Download Manager
Comment=Clean-room download manager with Tor (Arti)
Exec={exe_str}
Icon=rdownloader
Terminal=false
Categories=Network;FileTransfer;
StartupNotify=false
"#
        );
        std::fs::write(&path, desktop_entry)?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ───────────────────────── Windows Registry ─────────────────────────

#[cfg(target_os = "windows")]
fn set_windows_registry_autostart(enable: bool) -> Result<()> {
    let exe = current_exe();
    let exe_str = exe.to_string_lossy();
    if enable {
        let cmd = format!(
            r#"reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v "rdownloader" /t REG_SZ /d "\"{}\"" /f"#,
            exe_str
        );
        let _ = std::process::Command::new("cmd")
            .args(["/C", &cmd])
            .status();
    } else {
        let cmd = r#"reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v "rdownloader" /f"#;
        let _ = std::process::Command::new("cmd").args(["/C", cmd]).status();
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn check_windows_registry_autostart() -> bool {
    let cmd = r#"reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v "rdownloader""#;
    std::process::Command::new("cmd")
        .args(["/C", cmd])
        .output()
        .is_ok_and(|out| out.status.success())
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::*;

    #[test]
    fn autostart_paths_and_helpers() {
        #[cfg(target_os = "macos")]
        {
            let path = macos_launch_agent_path();
            assert!(path.is_some());
            let p = path.unwrap();
            assert!(p.to_string_lossy().contains("LaunchAgents"));
            assert!(p.to_string_lossy().ends_with("org.rdownloader.app.plist"));
        }

        #[cfg(target_os = "linux")]
        {
            let path = linux_xdg_autostart_path();
            assert!(path.is_some());
            let p = path.unwrap();
            assert!(p.to_string_lossy().contains("autostart"));
            assert!(p.to_string_lossy().ends_with("rdownloader.desktop"));
        }
    }
}
