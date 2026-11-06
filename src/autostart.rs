//! Launch at login, the way each OS does it for apps from a user's own account:
//! an XDG autostart entry on Linux, a LaunchAgent on macOS, the per-user Run
//! key on Windows. Nothing needs admin rights. The OS holds the setting, so
//! Tailgate reads it back from there instead of remembering it.
//!
//! When TAILGATE_CONFIG_DIR is set (tests, screenshots) everything goes in
//! that folder instead, so trying the app never touches the real login items.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli;

/// What Tailgate is started with at login: it opens minimized.
pub const LOGIN_FLAG: &str = "--login";
pub const MAC_LABEL: &str = "dev.centerpointlabs.tailgate";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Tailgate";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// A file to write: the autostart entry, the LaunchAgent, or the test stand-in.
    File(PathBuf),
    /// HKCU\...\Run, through reg.exe.
    WindowsRun,
}

pub fn place() -> Place {
    if let Some(dir) = std::env::var_os("TAILGATE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Place::File(PathBuf::from(dir).join("autostart").join(file_name()));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if cfg!(windows) {
        Place::WindowsRun
    } else if cfg!(target_os = "macos") {
        Place::File(home.join("Library/LaunchAgents").join(format!("{MAC_LABEL}.plist")))
    } else {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Place::File(config.join("autostart").join("tailgate.desktop"))
    }
}

fn file_name() -> String {
    if cfg!(target_os = "macos") {
        format!("{MAC_LABEL}.plist")
    } else if cfg!(windows) {
        "tailgate-run.txt".into()
    } else {
        "tailgate.desktop".into()
    }
}

/// The program to start at login: the AppImage itself when run from one, the
/// .app bundle on macOS, otherwise this executable.
pub fn launch_target() -> Option<PathBuf> {
    if let Some(appimage) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(appimage));
    }
    std::env::current_exe().ok()
}

pub fn is_enabled() -> bool {
    match place() {
        Place::File(path) => path.is_file(),
        Place::WindowsRun => cli::run(Path::new("reg"), &args(&["query", RUN_KEY, "/v", RUN_VALUE]), Duration::from_secs(5))
            .is_ok_and(|o| o.success),
    }
}

pub fn set_enabled(on: bool) -> Result<(), String> {
    let place = place();
    if !on {
        return match place {
            Place::File(path) => match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("Couldn't remove {}: {e}", path.display())),
            },
            Place::WindowsRun => reg(&["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]),
        };
    }
    let exe = launch_target().ok_or("Couldn't work out where Tailgate is installed.")?;
    match place {
        Place::File(path) => {
            let body = if cfg!(target_os = "macos") {
                launch_agent(&exe)
            } else if cfg!(windows) {
                windows_command(&exe)
            } else {
                desktop_entry(&exe)
            };
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
            }
            std::fs::write(&path, body).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
        }
        Place::WindowsRun => reg(&["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &windows_command(&exe), "/f"]),
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn reg(list: &[&str]) -> Result<(), String> {
    match cli::run(Path::new("reg"), &args(list), Duration::from_secs(10)) {
        Ok(out) if out.success => Ok(()),
        Ok(out) => Err(format!("reg.exe said: {}", out.stderr.trim())),
        Err(e) => Err(e.to_string()),
    }
}

/// An XDG autostart entry. GNOME, KDE, Cinnamon, Xfce and the rest read these.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Tailgate\nComment=Reconnect file shares over Tailscale\n\
         Exec={} {LOGIN_FLAG}\nIcon=tailgate\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        desktop_quote(&exe.to_string_lossy())
    )
}

/// Quoting for an Exec= line, per the Desktop Entry spec.
pub fn desktop_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        if c == '%' {
            out.push('%');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The .app bundle an executable lives in, if any.
pub fn mac_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// What launchd runs. From a bundle it goes through `open`, so macOS treats it
/// as the app (Dock icon, one copy only); `-g` keeps it from taking focus.
pub fn mac_program(exe: &Path) -> Vec<String> {
    match mac_bundle(exe) {
        Some(bundle) => vec![
            "/usr/bin/open".into(),
            "-g".into(),
            "-a".into(),
            bundle.to_string_lossy().into_owned(),
            "--args".into(),
            LOGIN_FLAG.into(),
        ],
        None => vec![exe.to_string_lossy().into_owned(), LOGIN_FLAG.into()],
    }
}

pub fn launch_agent(exe: &Path) -> String {
    let program: String = mac_program(exe).iter().map(|a| format!("        <string>{}</string>\n", xml_escape(a))).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20   <key>Label</key>\n    <string>{MAC_LABEL}</string>\n\
         \x20   <key>ProgramArguments</key>\n    <array>\n{program}    </array>\n\
         \x20   <key>RunAtLoad</key>\n    <true/>\n\
         \x20   <key>ProcessType</key>\n    <string>Interactive</string>\n\
         </dict>\n</plist>\n"
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The Run value: the quoted path and the flag.
pub fn windows_command(exe: &Path) -> String {
    format!("\"{}\" {LOGIN_FLAG}", exe.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_quotes_the_path() {
        let e = desktop_entry(Path::new("/usr/bin/tailgate"));
        assert!(e.contains("\nExec=/usr/bin/tailgate --login\n"), "{e}");
        assert!(e.starts_with("[Desktop Entry]\nType=Application\n"));
        assert_eq!(desktop_quote("/home/pat/My Apps/tailgate"), "\"/home/pat/My Apps/tailgate\"");
        assert_eq!(desktop_quote("/opt/a$b\"c"), "\"/opt/a\\$b\\\"c\"");
        assert_eq!(desktop_quote("/opt/100%"), "\"/opt/100%%\"");
    }

    #[test]
    fn mac_agent_opens_the_bundle_in_the_background() {
        let exe = Path::new("/Applications/Tailgate.app/Contents/MacOS/tailgate");
        assert_eq!(mac_bundle(exe), Some(PathBuf::from("/Applications/Tailgate.app")));
        assert_eq!(mac_program(exe), ["/usr/bin/open", "-g", "-a", "/Applications/Tailgate.app", "--args", "--login"]);
        let plist = launch_agent(exe);
        assert!(plist.contains("<string>dev.centerpointlabs.tailgate</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("        <string>/Applications/Tailgate.app</string>\n"));
        // A label of its own, so it can't collide with any other login item.
        assert_ne!(MAC_LABEL, "dev.centerpointlabs.mount-obsidian2");
        let loose = Path::new("/Users/pat/bin/tailgate");
        assert_eq!(mac_bundle(loose), None);
        assert_eq!(mac_program(loose), ["/Users/pat/bin/tailgate", "--login"]);
        assert!(launch_agent(Path::new("/tmp/a&b/t")).contains("/tmp/a&amp;b/t"));
    }

    #[test]
    fn windows_run_value_is_quoted() {
        assert_eq!(windows_command(Path::new(r"C:\Users\Pat\Tailgate\Tailgate.exe")), r#""C:\Users\Pat\Tailgate\Tailgate.exe" --login"#);
    }
}
