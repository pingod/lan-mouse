//! Starting lan-mouse with the user's session, and turning that on or off.
//!
//! Both platforms use the same shape: a small file in a well-known directory
//! that the OS reads at login. lan-mouse installs and removes it on request
//! rather than shipping a pre-made unit, so the path to the binary always
//! matches the copy the user is actually running.

use std::io;
use std::path::PathBuf;

/// Linux: `systemd --user` unit, `macOS`: launchd agent.
#[cfg(all(unix, not(target_os = "macos")))]
const UNIT_NAME: &str = "lan-mouse.service";
#[cfg(target_os = "macos")]
const UNIT_NAME: &str = "de.feschber.LanMouse.plist";

/// Whether autostart is currently installed.
pub(crate) fn is_enabled() -> bool {
    entry_path().map(|p| p.exists()).unwrap_or(false)
}

/// Install or remove the autostart entry.
pub(crate) fn set_enabled(enabled: bool) -> io::Result<()> {
    let path = entry_path()?;
    if enabled {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, contents()?)?;
        log::info!("autostart installed at {}", path.display());
        #[cfg(all(unix, not(target_os = "macos")))]
        reload_systemd();
    } else if path.exists() {
        std::fs::remove_file(&path)?;
        log::info!("autostart removed from {}", path.display());
        #[cfg(all(unix, not(target_os = "macos")))]
        reload_systemd();
    }
    Ok(())
}

/// Path of the entry for the current user.
fn entry_path() -> io::Result<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "no HOME"))?;
        Ok(base.join("systemd/user").join(UNIT_NAME))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "no HOME"))?;
        Ok(PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(UNIT_NAME))
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "autostart is not supported on this platform",
        ))
    }
}

/// The entry's contents, pointing at the running binary.
fn contents() -> io::Result<String> {
    let exe = std::env::current_exe()?;
    let exe = exe.display();

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(format!(
            "\
[Unit]
Description=Lan Mouse (mouse & keyboard sharing)
Documentation=https://github.com/feschber/lan-mouse
# lan-mouse needs an active graphical session
After=graphical-session.target
BindsTo=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart={exe} daemon
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
"
        ))
    }
    #[cfg(target_os = "macos")]
    {
        // Launch the GUI build, not `daemon`: only the GUI asks macOS for the
        // Accessibility / Input Monitoring permissions, so starting the daemon
        // directly can leave the app without them.
        Ok(format!(
            "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
    <key>Label</key>
    <string>de.feschber.LanMouse</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardErrorPath</key>
    <string>/tmp/lan-mouse.err.log</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = exe;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "autostart is not supported on this platform",
        ))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reload_systemd() {
    // `daemon-reload` is needed for systemd to notice a new or removed unit;
    // without it the entry only takes effect after the next login.
    match std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => log::warn!("systemctl --user daemon-reload exited with {s}"),
        Err(e) => log::warn!("could not run systemctl: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_path_is_under_a_config_dir() {
        let path = entry_path().expect("path");
        assert!(path.to_string_lossy().contains(UNIT_NAME));
    }

    #[test]
    fn contents_reference_the_current_binary() {
        // The entry must point at the binary that is running, otherwise
        // autostart would launch a stale or missing copy.
        let text = contents().expect("contents");
        let exe = std::env::current_exe().expect("exe").display().to_string();
        assert!(text.contains(&exe), "entry should reference {exe}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_entry_launches_the_gui() {
        // Only the GUI requests the privacy permissions; a daemon entry would
        // start without them.
        let text = contents().expect("contents");
        assert!(!text.contains("daemon"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_entry_runs_the_daemon() {
        let text = contents().expect("contents");
        assert!(text.contains("daemon"));
    }
}
