//! The only code that launches `tailscale`, and the safety boundary around it.
//!
//! Reads go through [`Query`] and writes through [`Action`], so nothing else
//! in the app can assemble an arbitrary command line. Read-only mode is
//! enforced in [`Client::mutate`], before anything is spawned.

use std::io::Read;
use std::net::IpAddr;
use std::os::unix::process::CommandExt;
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
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Missing(path) => write!(f, "Couldn't find {}. Is it installed?", path.display()),
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
        Client {
            custom_bin: bin_env.is_some(),
            custom_pkexec: pkexec_env.is_some(),
            bin: bin_env.map(PathBuf::from).unwrap_or_else(|| find_in_path("tailscale")),
            pkexec: pkexec_env.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("pkexec")),
            read_only,
            read_timeout,
            write_timeout: None,
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

fn find_in_path(name: &str) -> PathBuf {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
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
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
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
                // Signal the group so a shell wrapper's children die with it.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.kill();
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
pub fn current_user() -> Option<String> {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*pw).pw_name).to_str().ok().map(String::from)
    }
}

/// Opens an https URL with xdg-open and reaps the child off-thread.
pub fn open_url(url: &str) -> Result<(), CliError> {
    if !url.starts_with("https://") {
        return Err(CliError::Invalid(format!("Refusing to open '{url}'")));
    }
    let opener = std::env::var_os("TAILGATE_OPEN_BIN")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("xdg-open"));
    let mut child = Command::new(&opener)
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

    #[test]
    fn silent_failure_still_gets_a_message() {
        let out = Output { success: false, code: Some(3), stdout: String::new(), stderr: String::new() };
        assert_eq!(classify(&out), CliError::Failed("tailscale exited with status 3".into()));
    }
}
