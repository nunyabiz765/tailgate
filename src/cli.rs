//! The only code that launches `tailscale`, and the safety boundary around it.
//!
//! Reads go through [`Query`] and writes through [`Action`], so nothing else
//! in the app can assemble an arbitrary command line. Read-only mode is
//! enforced in [`Client::mutate`], before anything is spawned.

use std::ffi::OsString;
use std::io::Read;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const READ_TIMEOUT: Duration = Duration::from_secs(8);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(25);
/// Long enough for someone to find and type their password.
pub const PKEXEC_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    Status,
    Prefs,
    ServeText,
    ServeJson,
}

impl Query {
    pub fn args(self) -> Vec<String> {
        let args: &[&str] = match self {
            Query::Status => &["status", "--json"],
            Query::Prefs => &["debug", "prefs"],
            Query::ServeText => &["serve", "status"],
            Query::ServeJson => &["serve", "status", "--json"],
        };
        args.iter().map(|s| s.to_string()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `ip` is the node's Tailscale IP, the form `--exit-node` documents first.
    SetExitNode { ip: String, label: String },
    ClearExitNode,
    AllowLan(bool),
    Up,
    Down,
    SetOperator(String),
}

impl Action {
    pub fn args(&self) -> Result<Vec<String>, CliError> {
        Ok(match self {
            Action::SetExitNode { ip, .. } => {
                let ip: IpAddr = ip
                    .parse()
                    .map_err(|_| CliError::Invalid(format!("'{ip}' is not an IP address")))?;
                vec!["set".into(), format!("--exit-node={ip}")]
            }
            Action::ClearExitNode => vec!["set".into(), "--exit-node=".into()],
            Action::AllowLan(on) => vec!["set".into(), format!("--exit-node-allow-lan-access={on}")],
            // No flags on purpose: `up --help` says bare `up` brings the network
            // online "without changing any settings", and the check for
            // accidental setting reverts only applies when flags are passed.
            Action::Up => vec!["up".into()],
            Action::Down => vec!["down".into()],
            Action::SetOperator(user) => {
                if !valid_username(user) {
                    return Err(CliError::Invalid(format!("'{user}' is not a usable username")));
                }
                vec!["set".into(), format!("--operator={user}")]
            }
        })
    }

    /// Fills in "Tailgate tried to ..." and "Working: ...".
    pub fn describe(&self) -> String {
        match self {
            Action::SetExitNode { label, .. } => format!("switch the exit node to {label}"),
            Action::ClearExitNode => "turn the exit node off".into(),
            Action::AllowLan(true) => "turn on LAN access".into(),
            Action::AllowLan(false) => "turn off LAN access".into(),
            Action::Up => "bring Tailscale up".into(),
            Action::Down => "turn Tailscale off".into(),
            Action::SetOperator(user) => format!("make {user} the Tailscale operator"),
        }
    }
}

fn valid_username(user: &str) -> bool {
    let mut chars = user.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    Missing(PathBuf),
    AccessDenied(String),
    Timeout(Duration),
    Failed(String),
    Malformed(String),
    Invalid(String),
    /// Read-only mode stopped this before anything was spawned.
    ReadOnly(String),
    /// The password prompt was dismissed or refused.
    AuthDeclined,
    /// A test double is configured with no fake pkexec, so we won't run it as root.
    ElevationRefused,
    /// Windows and macOS have no pkexec; Tailgate never asks for admin rights there.
    ElevationUnavailable,
}

/// What Windows and macOS show when discovery finds no tailscale CLI anywhere.
pub const NOT_FOUND: &str =
    "Tailscale isn't installed, or Tailgate couldn't find it. Install it from tailscale.com/download, then reopen Tailgate.";
pub const DOWNLOAD_URL: &str = "https://tailscale.com/download";

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Missing(path) if cfg!(target_os = "linux") || path.file_stem() != Some("tailscale".as_ref()) => {
                write!(f, "Couldn't find {}. Is it installed?", path.display())
            }
            CliError::Missing(_) => write!(f, "{NOT_FOUND}"),
            CliError::AccessDenied(msg) => write!(f, "Tailscale refused: {msg}"),
            CliError::Timeout(t) => write!(f, "No answer after {} seconds.", t.as_secs()),
            CliError::Failed(msg) => write!(f, "{msg}"),
            CliError::Malformed(msg) => write!(f, "Couldn't read Tailscale's answer: {msg}"),
            CliError::Invalid(msg) => write!(f, "{msg}"),
            CliError::ReadOnly(cmd) => write!(f, "Read-only mode: did not run `{cmd}`."),
            CliError::AuthDeclined => write!(f, "Authorization was cancelled. Nothing changed."),
            CliError::ElevationRefused => {
                write!(f, "A test binary is configured, so Tailgate won't run it as root.")
            }
            CliError::ElevationUnavailable => write!(f, "Tailgate doesn't run Tailscale as administrator on this system."),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    pub bin: PathBuf,
    pub pkexec: PathBuf,
    pub read_only: bool,
    /// TAILGATE_TAILSCALE_BIN is set, so this isn't the system binary.
    pub custom_bin: bool,
    pub custom_pkexec: bool,
    pub read_timeout: Duration,
    pub write_timeout: Option<Duration>,
    /// Which kind of Tailscale install the binary belongs to.
    pub variant: Variant,
}

impl Client {
    pub fn from_env(read_only: bool) -> Client {
        let bin_env = std::env::var_os("TAILGATE_TAILSCALE_BIN").filter(|v| !v.is_empty());
        let pkexec_env = std::env::var_os("TAILGATE_PKEXEC_BIN").filter(|v| !v.is_empty());
        let read_timeout = std::env::var("TAILGATE_READ_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(READ_TIMEOUT);
        let found = match &bin_env {
            Some(bin) => Found { bin: PathBuf::from(bin), variant: Variant::Custom },
            None => discover(),
        };
        Client {
            custom_bin: bin_env.is_some(),
            custom_pkexec: pkexec_env.is_some(),
            bin: found.bin,
            pkexec: pkexec_env.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("pkexec")),
            read_only,
            read_timeout,
            write_timeout: None,
            variant: found.variant,
        }
    }

    /// For tests: a client aimed at a specific binary.
    pub fn with_bin(bin: impl Into<PathBuf>) -> Client {
        Client {
            bin: bin.into(),
            pkexec: PathBuf::from("pkexec"),
            read_only: false,
            custom_bin: true,
            custom_pkexec: false,
            read_timeout: READ_TIMEOUT,
            write_timeout: None,
            variant: Variant::Custom,
        }
    }

    pub fn read(&self, query: Query) -> Result<String, CliError> {
        let out = run(&self.bin, &query.args(), self.read_timeout)?;
        if out.success {
            Ok(out.stdout)
        } else {
            Err(classify(&out))
        }
    }

    pub fn mutate(&self, action: &Action, elevate: bool) -> Result<String, CliError> {
        let args = action.args()?;
        if self.read_only {
            return Err(CliError::ReadOnly(format!("tailscale {}", args.join(" "))));
        }
        if elevate {
            return self.elevated(&args);
        }
        let timeout = self.write_timeout.unwrap_or(WRITE_TIMEOUT);
        let out = run(&self.bin, &args, timeout)?;
        if out.success {
            Ok(out.stdout)
        } else {
            Err(classify(&out))
        }
    }

    fn elevated(&self, args: &[String]) -> Result<String, CliError> {
        if !ELEVATION && !self.custom_pkexec {
            return Err(CliError::ElevationUnavailable);
        }
        if self.custom_bin && !self.custom_pkexec {
            return Err(CliError::ElevationRefused);
        }
        let mut full = vec![self.bin.to_string_lossy().into_owned()];
        full.extend_from_slice(args);
        let out = run(&self.pkexec, &full, PKEXEC_TIMEOUT)?;
        if out.success {
            return Ok(out.stdout);
        }
        // pkexec: 126 = dialog dismissed, 127 = not authorized.
        match out.code {
            Some(126) | Some(127) => Err(CliError::AuthDeclined),
            _ => Err(classify(&out)),
        }
    }
}

/// pkexec exists only on Linux. Elsewhere an "access denied" gets an explanation
/// instead of a password prompt.
pub const ELEVATION: bool = cfg!(target_os = "linux");

/// The kind of Tailscale install found, which decides what Tailgate can promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// `tailscale` on PATH, as on Linux.
    Path,
    /// TAILGATE_TAILSCALE_BIN, or a test.
    Custom,
    /// tailscale.exe from the Windows installer.
    Windows,
    /// The macOS Standalone app from tailscale.com (its CLI works fully).
    MacStandalone,
    /// The Mac App Store app: sandboxed, its CLI only works while the app runs.
    MacAppStore,
    /// The open-source tailscaled, e.g. from Homebrew.
    MacOpenSource,
    /// Nothing found on Windows or macOS.
    NotFound,
}

impl Variant {
    /// A short sentence for the banner, or nothing when there's nothing to say.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Variant::NotFound => Some(NOT_FOUND),
            Variant::MacAppStore => Some(
                "Tailscale from the Mac App Store: Tailgate uses the command line built into that app, which \
                 only answers while the Tailscale app is running (keep its menu bar icon up). If a change \
                 is refused, the Standalone version from tailscale.com/download has the full command line.",
            ),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub bin: PathBuf,
    pub variant: Variant,
}

/// Where the system's tailscale CLI lives. Linux is the PATH lookup it always was.
pub fn discover() -> Found {
    let env = Probe {
        path: std::env::var_os("PATH"),
        home: std::env::var_os("HOME").map(PathBuf::from),
        program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
        is_file: &|p: &Path| p.is_file(),
    };
    if cfg!(windows) {
        discover_windows(&env)
    } else if cfg!(target_os = "macos") {
        discover_macos(&env)
    } else {
        match find_on(env.path.as_ref(), "tailscale", env.is_file) {
            Some(bin) => Found { bin, variant: Variant::Path },
            None => Found { bin: PathBuf::from("tailscale"), variant: Variant::Path },
        }
    }
}

/// Discovery's view of the system, so tests can fake every OS on any OS.
pub struct Probe<'a> {
    pub path: Option<OsString>,
    pub home: Option<PathBuf>,
    pub program_files: Option<PathBuf>,
    pub is_file: &'a dyn Fn(&Path) -> bool,
}

pub fn discover_windows(env: &Probe) -> Found {
    if let Some(bin) = find_on(env.path.as_ref(), "tailscale.exe", env.is_file) {
        return Found { bin, variant: Variant::Windows };
    }
    let mut dirs: Vec<PathBuf> = env.program_files.iter().cloned().collect();
    dirs.push(PathBuf::from(r"C:\Program Files"));
    for dir in dirs {
        let bin = dir.join("Tailscale").join("tailscale.exe");
        if (env.is_file)(&bin) {
            return Found { bin, variant: Variant::Windows };
        }
    }
    Found { bin: PathBuf::from("tailscale.exe"), variant: Variant::NotFound }
}

/// Apps opened from Finder get a bare PATH (/usr/bin:/bin:/usr/sbin:/sbin), so
/// the usual install spots are checked by name as well.
pub fn discover_macos(env: &Probe) -> Found {
    let mut apps = vec![PathBuf::from("/Applications/Tailscale.app")];
    if let Some(home) = &env.home {
        apps.push(home.join("Applications").join("Tailscale.app"));
    }
    let app = apps.into_iter().find(|app| (env.is_file)(&app.join("Contents/MacOS/Tailscale")));
    let app_variant = app.as_ref().map(|app| {
        if (env.is_file)(&app.join("Contents/_MASReceipt/receipt")) {
            Variant::MacAppStore
        } else {
            Variant::MacStandalone
        }
    });

    let mut search: Vec<PathBuf> = env.path.as_ref().map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    for extra in ["/usr/local/bin", "/opt/homebrew/bin"] {
        if !search.iter().any(|d| d == Path::new(extra)) {
            search.push(PathBuf::from(extra));
        }
    }
    let joined = std::env::join_paths(&search).ok();
    if let Some(bin) = find_on(joined.as_ref(), "tailscale", env.is_file) {
        // The Standalone app's "CLI integration" is a launcher script in
        // /usr/local/bin; with an app installed, the CLI is that app's.
        return Found { bin, variant: app_variant.unwrap_or(Variant::MacOpenSource) };
    }
    match (app, app_variant) {
        (Some(app), Some(variant)) => Found { bin: app.join("Contents/MacOS/Tailscale"), variant },
        _ => Found { bin: PathBuf::from("tailscale"), variant: Variant::NotFound },
    }
}

/// Whether `name` is a file somewhere on PATH.
pub fn on_path(name: &str) -> bool {
    find_on(std::env::var_os("PATH").as_ref(), name, &|p: &Path| p.is_file()).is_some()
}

fn find_on(paths: Option<&OsString>, name: &str, is_file: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    std::env::split_paths(paths?).map(|dir| dir.join(name)).find(|candidate| is_file(candidate))
}

fn classify(out: &Output) -> CliError {
    let message = first_useful(&out.stderr).or_else(|| first_useful(&out.stdout));
    let message = message.unwrap_or_else(|| match out.code {
        Some(code) => format!("tailscale exited with status {code}"),
        None => "tailscale was stopped by a signal".to_string(),
    });
    if message.to_lowercase().contains("access denied") {
        CliError::AccessDenied(message)
    } else {
        CliError::Failed(message)
    }
}

fn first_useful(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(400).collect())
    }
}

#[derive(Debug)]
pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a program with a hard deadline. Called from worker threads only.
pub fn run(program: &Path, args: &[String], timeout: Duration) -> Result<Output, CliError> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    // CREATE_NO_WINDOW: no console flashing up behind the window on every poll.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    // The Mac app's binary picks GUI or CLI from the environment; a GUI parent
    // looks like Finder, so say which one we want (Tailscale's documented switch).
    #[cfg(target_os = "macos")]
    command.env("TAILSCALE_BE_CLI", "1");
    let mut child = command
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CliError::Missing(program.to_path_buf()),
            _ => CliError::Failed(format!("Couldn't run {}: {e}", program.display())),
        })?;

    let stdout = collect(child.stdout.take());
    let stderr = collect(child.stderr.take());
    let started = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                kill_tree(&mut child);
                let _ = child.wait();
                return Err(CliError::Timeout(timeout));
            }
            Ok(None) => thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(CliError::Failed(format!("Lost track of {}: {e}", program.display()))),
        }
    };

    // A leftover grandchild can hold a pipe open, so don't wait on EOF forever.
    let grace = Instant::now();
    while !(stdout.is_done() && stderr.is_done()) && grace.elapsed() < Duration::from_millis(500) {
        thread::sleep(Duration::from_millis(5));
    }

    Ok(Output {
        success: status.success(),
        code: status.code(),
        stdout: stdout.text(),
        stderr: stderr.text(),
    })
}

/// Kills the child and anything it started, so a wrapper's children die with it.
fn kill_tree(child: &mut std::process::Child) {
    // Signal the group (the child leads its own, see `run`).
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL)
    };
    // No process groups to signal on Windows; taskkill /T walks the tree.
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .status();
    let _ = child.kill();
}

struct Collector {
    buf: Arc<Mutex<Vec<u8>>>,
    handle: thread::JoinHandle<()>,
}

impl Collector {
    fn is_done(&self) -> bool {
        self.handle.is_finished()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
    }
}

fn collect<R: Read + Send + 'static>(reader: Option<R>) -> Collector {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = buf.clone();
    let handle = thread::spawn(move || {
        let Some(mut reader) = reader else { return };
        let mut chunk = [0u8; 16384];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    Collector { buf, handle }
}

/// The logged-in user, from the password database rather than the environment.
#[cfg(unix)]
pub fn current_user() -> Option<String> {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*pw).pw_name).to_str().ok().map(String::from)
    }
}

/// Only used for the Linux operator grant, which Windows doesn't have.
#[cfg(not(unix))]
pub fn current_user() -> Option<String> {
    None
}

/// The system's "open this link" program: xdg-open, open, or Windows' URL handler.
fn default_opener() -> (PathBuf, &'static [&'static str]) {
    if cfg!(windows) {
        (PathBuf::from("rundll32"), &["url.dll,FileProtocolHandler"])
    } else if cfg!(target_os = "macos") {
        (PathBuf::from("/usr/bin/open"), &[])
    } else {
        (PathBuf::from("xdg-open"), &[])
    }
}

/// Opens an https URL in the browser and reaps the child off-thread.
pub fn open_url(url: &str) -> Result<(), CliError> {
    if !url.starts_with("https://") {
        return Err(CliError::Invalid(format!("Refusing to open '{url}'")));
    }
    let (opener, lead) = match std::env::var_os("TAILGATE_OPEN_BIN").filter(|v| !v.is_empty()) {
        Some(bin) => (PathBuf::from(bin), &[][..]),
        None => default_opener(),
    };
    let mut child = Command::new(&opener)
        .args(lead)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CliError::Missing(opener.clone()),
            _ => CliError::Failed(format!("Couldn't open a browser: {e}")),
        })?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_exit(ip: &str) -> Action {
        Action::SetExitNode { ip: ip.into(), label: "Dallas".into() }
    }

    #[test]
    fn builds_exact_mutation_args() {
        assert_eq!(set_exit("100.64.0.10").args().unwrap(), ["set", "--exit-node=100.64.0.10"]);
        assert_eq!(set_exit("fd7a:115c:a1e0::1").args().unwrap(), ["set", "--exit-node=fd7a:115c:a1e0::1"]);
        assert_eq!(Action::ClearExitNode.args().unwrap(), ["set", "--exit-node="]);
        assert_eq!(Action::AllowLan(true).args().unwrap(), ["set", "--exit-node-allow-lan-access=true"]);
        assert_eq!(Action::AllowLan(false).args().unwrap(), ["set", "--exit-node-allow-lan-access=false"]);
        assert_eq!(Action::Up.args().unwrap(), ["up"]);
        assert_eq!(Action::Down.args().unwrap(), ["down"]);
        assert_eq!(Action::SetOperator("pat".into()).args().unwrap(), ["set", "--operator=pat"]);
    }

    #[test]
    fn rejects_values_that_could_become_flags() {
        assert!(matches!(set_exit("--operator=evil").args(), Err(CliError::Invalid(_))));
        assert!(matches!(set_exit("").args(), Err(CliError::Invalid(_))));
        for bad in ["", "Pat", "a b", "-x", "a;b", "../x"] {
            assert!(matches!(Action::SetOperator(bad.into()).args(), Err(CliError::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn every_query_is_on_the_read_only_list() {
        for q in [Query::Status, Query::Prefs, Query::ServeText, Query::ServeJson] {
            let args = q.args();
            let head = args[0].as_str();
            assert!(matches!(head, "status" | "debug" | "serve"), "{args:?}");
            if head == "serve" {
                assert_eq!(args[1], "status", "serve must only ever be `serve status`");
            }
            if head == "debug" {
                assert_eq!(args, ["debug", "prefs"]);
            }
        }
    }

    #[test]
    fn access_denied_is_recognized_in_any_case() {
        let out = Output {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: "ACCESS DENIED: checkprefs access denied\n".into(),
        };
        assert!(matches!(classify(&out), CliError::AccessDenied(_)));
    }

    fn probe<'a>(path: Option<&str>, is_file: &'a dyn Fn(&Path) -> bool) -> Probe<'a> {
        Probe {
            path: path.map(OsString::from),
            home: Some(PathBuf::from("/Users/pat")),
            program_files: Some(PathBuf::from(r"D:\Apps")),
            is_file,
        }
    }

    fn has<'a>(files: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        move |p: &Path| {
            let p = p.to_string_lossy().replace('\\', "/");
            files.iter().any(|f| f.replace('\\', "/") == p)
        }
    }

    #[test]
    fn windows_finds_the_installer_copy_or_says_not_found() {
        let files = [r"C:\Program Files/Tailscale/tailscale.exe"];
        let f = has(&files);
        let found = discover_windows(&probe(None, &f));
        assert_eq!(found.variant, Variant::Windows);
        assert!(found.bin.to_string_lossy().replace('\\', "/").ends_with("Program Files/Tailscale/tailscale.exe"));

        let files = [r"D:\Apps/Tailscale/tailscale.exe"];
        let f = has(&files);
        let found = discover_windows(&probe(None, &f));
        assert!(found.bin.to_string_lossy().replace('\\', "/").starts_with("D:/Apps"), "{found:?}");

        let none: [&str; 0] = [];
        let f = has(&none);
        assert_eq!(discover_windows(&probe(None, &f)).variant, Variant::NotFound);
        assert!(Variant::NotFound.note().unwrap().contains("tailscale.com/download"));
    }

    #[cfg(unix)]
    #[test]
    fn macos_tells_the_three_variants_apart() {
        let standalone = ["/Applications/Tailscale.app/Contents/MacOS/Tailscale"];
        let f = has(&standalone);
        let found = discover_macos(&probe(Some("/usr/bin:/bin"), &f));
        assert_eq!(found, Found { bin: standalone[0].into(), variant: Variant::MacStandalone });

        // CLI integration installed: the launcher wins, the variant is still the app's.
        let launcher = ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/local/bin/tailscale"];
        let f = has(&launcher);
        let found = discover_macos(&probe(Some("/usr/bin:/bin"), &f));
        assert_eq!(found, Found { bin: "/usr/local/bin/tailscale".into(), variant: Variant::MacStandalone });

        let store = ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/Applications/Tailscale.app/Contents/_MASReceipt/receipt"];
        let f = has(&store);
        let found = discover_macos(&probe(Some("/usr/bin:/bin"), &f));
        assert_eq!(found.variant, Variant::MacAppStore);
        assert!(found.variant.note().unwrap().contains("App Store"));

        let user_app = ["/Users/pat/Applications/Tailscale.app/Contents/MacOS/Tailscale"];
        let f = has(&user_app);
        assert_eq!(discover_macos(&probe(None, &f)).bin, PathBuf::from(user_app[0]));

        let brew = ["/opt/homebrew/bin/tailscale"];
        let f = has(&brew);
        assert_eq!(discover_macos(&probe(Some("/usr/bin"), &f)).variant, Variant::MacOpenSource);

        let none: [&str; 0] = [];
        let f = has(&none);
        assert_eq!(discover_macos(&probe(Some("/usr/bin"), &f)).variant, Variant::NotFound);
    }

    #[test]
    fn standalone_and_linux_variants_have_nothing_to_explain() {
        for v in [Variant::Path, Variant::Custom, Variant::Windows, Variant::MacStandalone, Variant::MacOpenSource] {
            assert_eq!(v.note(), None);
        }
    }

    #[test]
    fn silent_failure_still_gets_a_message() {
        let out = Output { success: false, code: Some(3), stdout: String::new(), stderr: String::new() };
        assert_eq!(classify(&out), CliError::Failed("tailscale exited with status 3".into()));
    }
}
