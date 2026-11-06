//! SMB file shares on tailnet devices: finding them, connecting them the way
//! each OS expects, and noticing which ones are already connected.
//!
//! Tailgate never sees or stores a password. Connecting goes through the OS's
//! own SMB client, which looks the sign-in up in the OS keychain (macOS
//! Keychain, Windows Credential Manager, the Secret Service on Linux). When
//! there's no saved sign-in, Tailgate hands the share to the file manager,
//! which asks in its own dialog and can remember the answer there.
//!
//! As in `cli`, every command line is built here from validated parts and run
//! without a shell, so a share or host name can't turn into a flag or a second
//! command.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{self, CliError, Output};

pub const SMB_PORT: u16 = 445;
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
pub const LIST_TIMEOUT: Duration = Duration::from_secs(12);
/// `gio mount` and `net use` wait for the server, which can be slow just after wake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
pub const MOUNTS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Mac,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }

    /// Where sign-ins end up, for the footer and messages.
    pub fn keychain(self) -> &'static str {
        match self {
            Os::Mac => "your Mac's Keychain",
            Os::Windows => "Windows Credential Manager",
            Os::Linux => "your desktop's keyring",
        }
    }

    pub fn file_manager(self) -> &'static str {
        match self {
            Os::Mac => "Finder",
            Os::Windows => "File Explorer",
            Os::Linux => "your file manager",
        }
    }
}

// ---------------------------------------------------------------------------
// Names

/// One share on one host, with an optional user to sign in as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRef {
    pub host: String,
    pub share: String,
    /// Empty means "whatever the OS uses by default".
    pub user: String,
}

impl ShareRef {
    pub fn new(host: &str, share: &str, user: &str) -> Result<ShareRef, String> {
        let host = host.trim();
        let share = share.trim().trim_matches(|c| c == '/' || c == '\\');
        let user = user.trim();
        if !valid_host(host) {
            return Err(if host.is_empty() {
                "Type the device's name or Tailscale IP.".into()
            } else {
                format!("'{host}' isn't a device name or IP address.")
            });
        }
        if !valid_share(share) {
            return Err(if share.is_empty() {
                "Type the share's name.".into()
            } else {
                format!("'{share}' can't be a share name.")
            });
        }
        if !user.is_empty() && !valid_user(user) {
            return Err(format!("'{user}' can't be a user name."));
        }
        Ok(ShareRef { host: host.to_string(), share: share.to_string(), user: user.to_string() })
    }

    /// Identity for matching and saving: host and share, ignoring case (SMB does).
    pub fn key(&self) -> String {
        share_key(&self.host, &self.share)
    }

    /// `smb://user@host/Share%20Name`, the form Finder and gio take.
    pub fn url(&self) -> String {
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", encode(&self.user)) };
        format!("smb://{user}{}/{}", url_host(&self.host), encode(&self.share))
    }

    /// `\\host\Share`, the form Windows takes.
    pub fn unc(&self) -> String {
        format!(r"\\{}\{}", unc_host(&self.host), self.share)
    }
}

/// Reads `smb://user@host/Share`, `//host/Share` or `\\host\Share`.
pub fn parse_location(text: &str) -> Result<ShareRef, String> {
    let text = text.trim();
    let rest = text.strip_prefix("smb:").unwrap_or(text);
    let rest = rest.trim_start_matches(['/', '\\']);
    let (authority, share) = rest.split_once(['/', '\\']).ok_or_else(|| format!("'{text}' doesn't name a share, like //host/Share."))?;
    let (user, host) = split_user(authority);
    ShareRef::new(&host, &decode(share), &user)
}

pub fn share_key(host: &str, share: &str) -> String {
    format!("{}/{}", host.trim().to_lowercase(), share.trim().to_lowercase())
}

pub fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['-', '.'])
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Windows' rules for share names, plus no leading dash and nothing invisible.
pub fn valid_share(share: &str) -> bool {
    !share.is_empty()
        && share.chars().count() <= 80
        && !share.starts_with('-')
        && share.chars().all(|c| !c.is_control() && !r#"/\[]:|<>+=;,*?""#.contains(c))
}

pub fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 104
        && !user.starts_with('-')
        && user.chars().all(|c| c.is_ascii_alphanumeric() || "._-@".contains(c))
}

fn url_host(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => format!("[{v6}]"),
        _ => host.to_string(),
    }
}

/// UNC paths can't hold a colon, so Windows spells IPv6 hosts this way.
fn unc_host(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => format!("{}.ipv6-literal.net", v6.to_string().replace(':', "-")),
        _ => host.to_string(),
    }
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
pub fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = text.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Programs

/// A program and its arguments, built here and nowhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Cmd {
    fn new(program: &Path, args: &[&str]) -> Cmd {
        Cmd { program: program.to_path_buf(), args: args.iter().map(|s| s.to_string()).collect() }
    }

    pub fn run(&self, timeout: Duration) -> Result<Output, CliError> {
        cli::run(&self.program, &self.args, timeout)
    }

    pub fn display(&self) -> String {
        let name = self.program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{name} {}", self.args.join(" "))
    }
}

/// The programs the share code runs, so tests can swap in fakes.
#[derive(Debug, Clone)]
pub struct Tools {
    pub os: Os,
    pub read_only: bool,
    /// TAILGATE_SHARES_TOOLS is set: fakes, and no real port probing.
    pub fake: bool,
    pub gio: PathBuf,
    pub smbclient: PathBuf,
    pub xdg_open: PathBuf,
    pub open: PathBuf,
    pub smbutil: PathBuf,
    pub mount: PathBuf,
    pub diskutil: PathBuf,
    pub umount: PathBuf,
    pub net: PathBuf,
    pub explorer: PathBuf,
    pub probe: PathBuf,
    pub proc_mounts: PathBuf,
    pub gvfs_dir: Option<PathBuf>,
}

impl Tools {
    pub fn system(read_only: bool) -> Tools {
        let dir = std::env::var_os("TAILGATE_SHARES_TOOLS").filter(|v| !v.is_empty()).map(PathBuf::from);
        Tools::with(Os::current(), read_only, dir)
    }

    /// `dir`: a directory of fakes named after the real programs (`.cmd` on Windows).
    pub fn with(os: Os, read_only: bool, dir: Option<PathBuf>) -> Tools {
        let fake = dir.is_some();
        let pick = |real: &str, name: &str| match &dir {
            Some(d) if cfg!(windows) => d.join(format!("{name}.cmd")),
            Some(d) => d.join(name),
            None => PathBuf::from(real),
        };
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let net = root.join("System32").join("net.exe");
        let explorer = root.join("explorer.exe");
        Tools {
            os,
            read_only,
            fake,
            gio: pick("gio", "gio"),
            smbclient: pick("smbclient", "smbclient"),
            xdg_open: pick("xdg-open", "xdg-open"),
            open: pick("/usr/bin/open", "open"),
            smbutil: pick("/usr/bin/smbutil", "smbutil"),
            mount: pick("/sbin/mount", "mount"),
            diskutil: pick("/usr/sbin/diskutil", "diskutil"),
            umount: pick("/sbin/umount", "umount"),
            net: pick(&net.to_string_lossy(), "net"),
            explorer: pick(&explorer.to_string_lossy(), "explorer"),
            probe: pick("", "probe"),
            proc_mounts: match &dir {
                Some(d) => d.join("proc-mounts"),
                None => PathBuf::from("/proc/self/mounts"),
            },
            gvfs_dir: match &dir {
                Some(d) => Some(d.join("gvfs")),
                None => std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("gvfs")),
            },
        }
    }

    /// Ways to ask a host for its share names, tried in order. They only use a
    /// sign-in the OS already has; none of them can prompt.
    pub fn list_cmds(&self, host: &str, user: &str) -> Vec<Cmd> {
        let at = if user.is_empty() { String::new() } else { format!("{}@", encode(user)) };
        match self.os {
            Os::Linux => vec![
                // Anonymous listing: Samba allows it unless `restrict anonymous` is set.
                Cmd::new(&self.smbclient, &["-L", &format!("//{}", url_host(host)), "-N", "-g", "-t", "8"]),
                // gvfs signs in with a password saved in the keyring, if there is one.
                Cmd::new(&self.gio, &["list", &format!("smb://{at}{}/", url_host(host))]),
            ],
            // -N: never prompt; smbutil uses the Keychain entry for user@host.
            Os::Mac => vec![Cmd::new(&self.smbutil, &["view", "-N", &format!("//{at}{}", url_host(host))])],
            Os::Windows => vec![Cmd::new(&self.net, &["view", &format!(r"\\{}", unc_host(host))])],
        }
    }

    /// Connects without asking anything; a missing sign-in comes back as
    /// [`Outcome::NeedsSignIn`].
    pub fn connect_cmd(&self, share: &ShareRef) -> Cmd {
        match self.os {
            Os::Linux => Cmd::new(&self.gio, &["mount", &share.url()]),
            // -g: in the background. Finder signs in from the Keychain, or asks.
            Os::Mac => Cmd::new(&self.open, &["-g", &share.url()]),
            // `*` takes the next free drive letter. Not persistent: Tailgate
            // reconnects at login itself, so Windows shouldn't also try at boot.
            Os::Windows => Cmd::new(&self.net, &["use", "*", &share.unc(), "/persistent:no"]),
        }
    }

    /// Hands the share to the file manager, which asks for a sign-in in the
    /// OS's own dialog and can save it to the keychain.
    pub fn sign_in_cmd(&self, share: &ShareRef) -> Cmd {
        match self.os {
            Os::Linux => Cmd::new(&self.xdg_open, &[&share.url()]),
            Os::Mac => Cmd::new(&self.open, &[&share.url()]),
            Os::Windows => Cmd::new(&self.explorer, &[&share.unc()]),
        }
    }

    pub fn disconnect_cmd(&self, mounted: &Mounted) -> Option<Cmd> {
        match (self.os, &mounted.kind) {
            (Os::Linux, MountKind::Gvfs) => Some(Cmd::new(&self.gio, &["mount", "-u", &mounted.as_ref().url()])),
            (Os::Mac, _) if !mounted.location.is_empty() => {
                Some(Cmd::new(&self.diskutil, &["unmount", &mounted.location]))
            }
            (Os::Windows, _) => {
                let target = if mounted.location.is_empty() { mounted.as_ref().unc() } else { mounted.location.clone() };
                Some(Cmd::new(&self.net, &["use", &target, "/delete", "/y"]))
            }
            // A cifs mount from fstab or root: not Tailgate's to undo.
            _ => None,
        }
    }

    /// Opens a connected share's folder.
    pub fn open_cmd(&self, mounted: &Mounted) -> Cmd {
        let target = match (self.os, &mounted.kind) {
            (Os::Windows, _) if mounted.location.len() == 2 => format!(r"{}\", mounted.location),
            (Os::Windows, _) if mounted.location.is_empty() => mounted.as_ref().unc(),
            (Os::Linux, MountKind::Gvfs) if mounted.location.is_empty() => mounted.as_ref().url(),
            _ => mounted.location.clone(),
        };
        match self.os {
            Os::Linux => Cmd::new(&self.xdg_open, &[&target]),
            Os::Mac => Cmd::new(&self.open, &[&target]),
            Os::Windows => Cmd::new(&self.explorer, &[&target]),
        }
    }
}

// ---------------------------------------------------------------------------
// Finding shares

/// A tailnet device worth asking about file shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub host: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub host: String,
    pub name: String,
    /// `None`: port 445 answered but no listing worked (usually needs a sign-in).
    pub shares: Option<Vec<String>>,
}

/// Whether the host takes TCP connections on 445. Never sends any SMB.
pub fn probe(tools: &Tools, host: &str) -> bool {
    if tools.fake {
        return Cmd::new(&tools.probe, &[host]).run(PROBE_TIMEOUT * 4).is_ok_and(|o| o.success);
    }
    let Ok(ip) = host.parse::<IpAddr>() else { return false };
    TcpStream::connect_timeout(&SocketAddr::new(ip, SMB_PORT), PROBE_TIMEOUT).is_ok()
}

/// Share names from the first listing that works, sorted, without `$` admin shares.
pub fn list_shares(tools: &Tools, host: &str, user: &str) -> Option<Vec<String>> {
    for cmd in tools.list_cmds(host, user) {
        let Ok(out) = cmd.run(LIST_TIMEOUT) else { continue };
        if !out.success {
            continue;
        }
        let shares = match (tools.os, cmd.args.first().map(String::as_str)) {
            (Os::Linux, Some("-L")) => parse_smbclient(&out.stdout),
            (Os::Linux, _) => parse_gio_list(&out.stdout),
            _ => parse_share_table(&out.stdout),
        };
        if !shares.is_empty() {
            return Some(shares);
        }
    }
    None
}

/// Probes every candidate at once, then lists shares on the ones that answered.
pub fn scan(tools: &Tools, candidates: &[Candidate]) -> Vec<Found> {
    let handles: Vec<_> = candidates
        .iter()
        .cloned()
        .map(|c| {
            let tools = tools.clone();
            thread::spawn(move || {
                if !probe(&tools, &c.host) {
                    return None;
                }
                let shares = list_shares(&tools, &c.host, "");
                Some(Found { host: c.host, name: c.name, shares })
            })
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
}

/// `smbclient -g -L`: `Disk|Name|Comment` lines.
pub fn parse_smbclient(text: &str) -> Vec<String> {
    let names = text.lines().filter_map(|line| {
        let mut parts = line.splitn(3, '|');
        match (parts.next(), parts.next()) {
            (Some("Disk"), Some(name)) => Some(name.to_string()),
            _ => None,
        }
    });
    tidy(names)
}

/// `gio list smb://host/`: one share per line.
pub fn parse_gio_list(text: &str) -> Vec<String> {
    tidy(text.lines().map(|l| decode(l.trim().trim_end_matches('/'))))
}

/// `smbutil view` and `net view`: a header naming a Type column, dashes, then
/// rows. Names can hold spaces, so the Type column's position splits them.
pub fn parse_share_table(text: &str) -> Vec<String> {
    let mut type_col = None;
    let mut in_rows = false;
    let mut names = Vec::new();
    for line in text.lines() {
        if !in_rows {
            if let Some(col) = line.find("Type") {
                type_col = Some(col);
            } else if line.trim_start().starts_with("---") && type_col.is_some() {
                in_rows = true;
            }
            continue;
        }
        let Some(col) = type_col else { break };
        if line.trim().is_empty() {
            continue;
        }
        let (name, rest) = if line.len() > col && line.is_char_boundary(col) && line[..col].ends_with(' ') {
            line.split_at(col)
        } else {
            // A long name pushes the type over; fall back to the last " Disk".
            match line.rfind(" Disk") {
                Some(i) => line.split_at(i + 1),
                None => continue,
            }
        };
        if rest.split_whitespace().next() == Some("Disk") {
            names.push(name.trim().to_string());
        }
    }
    tidy(names.into_iter())
}

fn tidy(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = names.filter(|n| !n.is_empty() && !n.ends_with('$') && valid_share(n)).collect();
    out.sort_by_key(|n| n.to_lowercase());
    out.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    out
}

// ---------------------------------------------------------------------------
// What's connected

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountKind {
    /// gvfs (`gio mount`), the kind Tailgate makes on Linux.
    Gvfs,
    /// A kernel cifs mount (fstab, mount.cifs): shown, never undone.
    Cifs,
    /// Finder's smbfs.
    Smbfs,
    /// A Windows network drive.
    NetUse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounted {
    pub host: String,
    pub share: String,
    pub user: String,
    /// A folder, or a drive letter like `Z:`. Empty for a gvfs mount without gvfs-fuse.
    pub location: String,
    pub kind: MountKind,
}

impl Mounted {
    /// Whether this is `share`, given every name its host goes by.
    pub fn is(&self, share: &str, host_aliases: &[String]) -> bool {
        self.share.eq_ignore_ascii_case(share) && host_aliases.iter().any(|h| h.eq_ignore_ascii_case(&self.host))
    }

    fn as_ref(&self) -> ShareRef {
        ShareRef { host: self.host.clone(), share: self.share.clone(), user: self.user.clone() }
    }
}

/// Everything SMB that's connected now. Errors just mean nothing found.
pub fn list_mounted(tools: &Tools) -> Vec<Mounted> {
    match tools.os {
        Os::Linux => {
            let mut all = Vec::new();
            if let Ok(out) = Cmd::new(&tools.gio, &["mount", "-l"]).run(MOUNTS_TIMEOUT) {
                let fuse = tools.gvfs_dir.as_deref().map(gvfs_folders).unwrap_or_default();
                all.extend(parse_gio_mounts(&out.stdout, &fuse));
            }
            if let Ok(text) = std::fs::read_to_string(&tools.proc_mounts) {
                all.extend(parse_proc_mounts(&text));
            }
            all
        }
        Os::Mac => Cmd::new(&tools.mount, &[]).run(MOUNTS_TIMEOUT).map(|o| parse_mac_mounts(&o.stdout)).unwrap_or_default(),
        Os::Windows => Cmd::new(&tools.net, &["use"]).run(MOUNTS_TIMEOUT).map(|o| parse_net_use(&o.stdout)).unwrap_or_default(),
    }
}

/// gvfs-fuse's folders (`smb-share:server=h,share=s,user=u`), so a connected
/// share can open as a normal folder. Missing when gvfs-fuse isn't running.
pub fn gvfs_folders(dir: &Path) -> Vec<(String, String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (host, share) = parse_gvfs_folder(&name)?;
            Some((host, share, e.path()))
        })
        .collect()
}

pub fn parse_gvfs_folder(name: &str) -> Option<(String, String)> {
    let spec = name.strip_prefix("smb-share:")?;
    let mut host = None;
    let mut share = None;
    for pair in spec.split(',') {
        match pair.split_once('=') {
            Some(("server", v)) => host = Some(decode(v)),
            Some(("share", v)) => share = Some(decode(v)),
            _ => {}
        }
    }
    Some((host?, share?))
}

/// Splits `user@host` (and gvfs's `domain;user@host`).
fn split_user(authority: &str) -> (String, String) {
    match authority.rsplit_once('@') {
        Some((user, host)) => {
            let user = user.rsplit(';').next().unwrap_or(user);
            let user = user.split(':').next().unwrap_or(user);
            (decode(user), host.trim_matches(['[', ']']).to_string())
        }
        None => (String::new(), authority.trim_matches(['[', ']']).to_string()),
    }
}

/// `gio mount -l`: `Mount(0): home on 100.64.0.1 -> smb://100.64.0.1/home/`.
pub fn parse_gio_mounts(text: &str, fuse: &[(String, String, PathBuf)]) -> Vec<Mounted> {
    text.lines()
        .filter_map(|line| {
            let url = line.split(" -> ").nth(1)?.trim();
            let rest = url.strip_prefix("smb://")?;
            let mut parts = rest.trim_end_matches('/').splitn(3, '/');
            let (user, host) = split_user(parts.next()?);
            let share = decode(parts.next()?);
            if host.is_empty() || share.is_empty() {
                return None;
            }
            let location = fuse
                .iter()
                .find(|(h, s, _)| h.eq_ignore_ascii_case(&host) && s.eq_ignore_ascii_case(&share))
                .map(|(_, _, p)| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            Some(Mounted { host, share, user, location, kind: MountKind::Gvfs })
        })
        .collect()
}

/// `/proc/self/mounts` lines for cifs/smb3, with the kernel's octal escapes.
pub fn parse_proc_mounts(text: &str) -> Vec<Mounted> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (source, point, fstype) = (f.next()?, f.next()?, f.next()?);
            if fstype != "cifs" && fstype != "smb3" {
                return None;
            }
            let source = unoctal(source);
            let rest = source.strip_prefix("//")?;
            let (host, share) = rest.split_once('/')?;
            let share = share.split('/').next()?.to_string();
            Some(Mounted { host: host.to_string(), share, user: String::new(), location: unoctal(point), kind: MountKind::Cifs })
        })
        .collect()
}

fn unoctal(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if let Some(b) = text.get(i + 1..i + 4).and_then(|s| u8::from_str_radix(s, 8).ok()) {
                out.push(b);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// macOS `mount`: `//nick@100.64.0.1/Home on /Volumes/Home (smbfs, nodev, ...)`.
pub fn parse_mac_mounts(text: &str) -> Vec<Mounted> {
    text.lines()
        .filter_map(|line| {
            let (source, rest) = line.split_once(" on ")?;
            let (point, opts) = rest.rsplit_once(" (")?;
            if !opts.starts_with("smbfs") {
                return None;
            }
            let rest = source.strip_prefix("//")?;
            let (authority, share) = rest.split_once('/')?;
            let (user, host) = split_user(authority);
            Some(Mounted { host, share: decode(share.trim_end_matches('/')), user, location: point.to_string(), kind: MountKind::Smbfs })
        })
        .collect()
}

/// `net use`: rows like `OK   Z:   \\host\Share   Microsoft Windows Network`.
/// Only `OK` rows count; Unavailable/Disconnected drives aren't usable.
pub fn parse_net_use(text: &str) -> Vec<Mounted> {
    text.lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let remote_at = words.iter().position(|w| w.starts_with(r"\\"))?;
            if words.first() != Some(&"OK") {
                return None;
            }
            let local = words[..remote_at].iter().find(|w| is_drive(w)).map(|w| w.to_uppercase()).unwrap_or_default();
            let remote = words[remote_at].trim_start_matches('\\');
            let (host, share) = remote.split_once('\\')?;
            let share = share.split('\\').next()?.to_string();
            if share.ends_with('$') {
                return None;
            }
            Some(Mounted { host: host.to_string(), share, user: String::new(), location: local, kind: MountKind::NetUse })
        })
        .collect()
}

fn is_drive(word: &str) -> bool {
    let b = word.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Drive letters with a stale (not OK) mapping to the share, to clear before reconnecting.
pub fn stale_drives(net_use: &str, share: &ShareRef) -> Vec<String> {
    let unc = share.unc().to_lowercase();
    net_use
        .lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let remote = words.iter().position(|w| w.to_lowercase() == unc)?;
            if words.first() == Some(&"OK") {
                return None;
            }
            words[..remote].iter().find(|w| is_drive(w)).map(|w| w.to_uppercase())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Connecting

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Connected; the drive letter when the tool said.
    Connected(String),
    /// Handed to Finder, which connects (or asks) by itself. Watch the mounts.
    Requested,
    /// The OS has no usable sign-in for it.
    NeedsSignIn(String),
    /// This system's SMB client can't mount it (no gvfs-smb, say).
    Unsupported(String),
    Failed(String),
}

/// What [`connect`] did about an outcome that needs the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connected {
    pub outcome: Outcome,
    /// The file manager was opened so the OS could ask for a sign-in or show the share.
    pub handed_off: bool,
}

/// Connects without prompting. If the OS has no sign-in, or can't mount SMB
/// itself, and `ask` is set (a click, not an automatic reconnect), the share
/// goes to the file manager, which asks in the OS's own dialog.
pub fn connect(tools: &Tools, share: &ShareRef, ask: bool) -> Connected {
    if tools.read_only {
        let outcome = Outcome::Failed(format!("Read-only mode: did not connect {}.", share.share));
        return Connected { outcome, handed_off: false };
    }
    if tools.os == Os::Windows {
        // A drive left "Unavailable" by sleep blocks a fresh `net use` of the same share.
        if let Ok(out) = Cmd::new(&tools.net, &["use"]).run(MOUNTS_TIMEOUT) {
            for drive in stale_drives(&out.stdout, share) {
                let _ = Cmd::new(&tools.net, &["use", &drive, "/delete", "/y"]).run(MOUNTS_TIMEOUT);
            }
        }
    }
    let outcome = match tools.connect_cmd(share).run(CONNECT_TIMEOUT) {
        Ok(out) => classify(tools.os, &out),
        Err(CliError::Missing(p)) if tools.os == Os::Linux => Outcome::Unsupported(format!("{} isn't installed", p.display())),
        Err(CliError::Timeout(t)) => Outcome::Failed(format!("No answer from {} after {} seconds.", share.host, t.as_secs())),
        Err(e) => Outcome::Failed(e.to_string()),
    };
    let wants_hand_off = matches!(outcome, Outcome::NeedsSignIn(_) | Outcome::Unsupported(_));
    if !(ask && wants_hand_off) {
        return Connected { outcome, handed_off: false };
    }
    match tools.sign_in_cmd(share).run(Duration::from_secs(15)) {
        Ok(out) if out.success || tools.os == Os::Windows => Connected { outcome, handed_off: true },
        Ok(_) | Err(_) => Connected { outcome, handed_off: false },
    }
}

pub fn classify(os: Os, out: &Output) -> Outcome {
    let text = format!("{}\n{}", out.stderr.trim(), out.stdout.trim()).trim().to_string();
    let first: String = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(300).collect();
    let lower = text.to_lowercase();
    match os {
        Os::Linux => {
            if out.success || lower.contains("already mounted") {
                Outcome::Connected(String::new())
            } else if ["not supported", "not mountable", "doesn't implement mount", "not implemented"]
                .iter()
                .any(|s| lower.contains(s))
            {
                Outcome::Unsupported(first)
            } else if ["password", "permission denied", "access denied", "logon", "authentication", "credentials"]
                .iter()
                .any(|s| lower.contains(s))
            {
                Outcome::NeedsSignIn(first)
            } else {
                Outcome::Failed(if first.is_empty() { "gio couldn't mount it.".into() } else { first })
            }
        }
        Os::Mac => {
            if out.success {
                Outcome::Requested
            } else {
                Outcome::Failed(if first.is_empty() { "Finder couldn't open it.".into() } else { first })
            }
        }
        Os::Windows => {
            if out.success {
                let drive = text.split_whitespace().find(|w| is_drive(w)).map(|w| w.to_uppercase()).unwrap_or_default();
                return Outcome::Connected(drive);
            }
            // The words are localized, the error numbers aren't. A dotted IP doesn't parse as one number.
            let codes: Vec<u32> = text
                .split_whitespace()
                .filter_map(|w| w.trim_end_matches(['.', ':', ',', ')']).trim_start_matches('(').parse().ok())
                .collect();
            // 5 access denied, 86 bad password, 1326 bad user or password, 1244 not signed in, 2202 bad user name.
            if codes.iter().any(|c| [5, 86, 1326, 1244, 2202].contains(c)) {
                Outcome::NeedsSignIn(first)
            } else if codes.contains(&1219) {
                Outcome::Failed("Windows is already connected to that device as a different user. Disconnect that first.".into())
            } else {
                Outcome::Failed(if first.is_empty() { "net use failed.".into() } else { first })
            }
        }
    }
}

pub fn disconnect(tools: &Tools, mounted: &Mounted) -> Result<(), String> {
    if tools.read_only {
        return Err(format!("Read-only mode: did not disconnect {}.", mounted.share));
    }
    let Some(cmd) = tools.disconnect_cmd(mounted) else {
        return Err(format!("{} was connected by the system, not Tailgate, so disconnect it there.", mounted.share));
    };
    let first = match cmd.run(CONNECT_TIMEOUT) {
        Ok(out) if out.success => return Ok(()),
        Ok(out) => first_line(&out).unwrap_or_else(|| format!("{} failed.", cmd.display())),
        Err(e) => e.to_string(),
    };
    // A share Finder has only just mounted is often still busy for a moment
    // (Spotlight, the Finder window it opened). One more try with plain umount,
    // which never forces anything.
    if tools.os == Os::Mac {
        thread::sleep(Duration::from_secs(2));
        if let Ok(out) = Cmd::new(&tools.umount, &[&mounted.location]).run(CONNECT_TIMEOUT) {
            if out.success {
                return Ok(());
            }
        }
        return Err(format!("{first}. Close any windows or files using it and try again."));
    }
    Err(first)
}

pub fn open(tools: &Tools, mounted: &Mounted) -> Result<(), String> {
    match tools.open_cmd(mounted).run(Duration::from_secs(15)) {
        // explorer.exe exits 1 even when it worked.
        Ok(out) if out.success || tools.os == Os::Windows => Ok(()),
        Ok(out) => Err(first_line(&out).unwrap_or_else(|| "Couldn't open it.".into())),
        Err(e) => Err(e.to_string()),
    }
}

fn first_line(out: &Output) -> Option<String> {
    format!("{}\n{}", out.stderr, out.stdout).lines().map(str::trim).find(|l| !l.is_empty()).map(String::from)
}

// ---------------------------------------------------------------------------
// Reconnecting

/// Waits after each automatic attempt at one share. Short at first (a share
/// often needs a few seconds after Tailscale reconnects), then slower, then it
/// stops until the next time Tailscale comes back.
pub const BACKOFF: [Duration; 6] = [
    Duration::from_secs(5),
    Duration::from_secs(20),
    Duration::from_secs(60),
    Duration::from_secs(180),
    Duration::from_secs(600),
    Duration::from_secs(1800),
];

#[derive(Debug, Clone, Default)]
struct Round {
    attempts: usize,
    next_at: Option<Instant>,
}

/// Decides when to retry "Connect at login" shares. Nothing is attempted until
/// [`Reconnect::trigger`] (Tailgate starting, Tailscale coming back, waking from
/// sleep, the host coming online); then each missing share gets up to
/// `BACKOFF.len()` tries, further and further apart, and then it waits for the
/// next trigger. A share that needs a sign-in, or that was disconnected by
/// hand, is held until the user clicks Connect.
#[derive(Debug, Default)]
pub struct Reconnect {
    rounds: BTreeMap<String, Round>,
    held: BTreeMap<String, &'static str>,
}

/// One "Connect at login" share as the scheduler sees it.
pub struct Want<'a> {
    pub key: &'a str,
    pub connected: bool,
    /// The host is online in Tailscale (or isn't a tailnet device we can see).
    pub reachable: bool,
}

impl Reconnect {
    /// Starts a fresh round of attempts for these shares.
    pub fn trigger<'a>(&mut self, now: Instant, keys: impl IntoIterator<Item = &'a str>) {
        for key in keys {
            if !self.held.contains_key(key) {
                self.rounds.insert(key.to_string(), Round { attempts: 0, next_at: Some(now) });
            }
        }
    }

    /// Shares to try now. Each one returned counts as an attempt.
    pub fn due(&mut self, now: Instant, wants: &[Want]) -> Vec<String> {
        let mut out = Vec::new();
        for want in wants {
            if want.connected {
                self.rounds.remove(want.key);
                continue;
            }
            let Some(round) = self.rounds.get_mut(want.key) else { continue };
            // An offline host costs nothing to wait for and doesn't use up a try.
            if !want.reachable {
                continue;
            }
            match round.next_at {
                Some(at) if at <= now && round.attempts < BACKOFF.len() => {
                    round.next_at = Some(now + BACKOFF[round.attempts]);
                    round.attempts += 1;
                    out.push(want.key.to_string());
                }
                _ => {}
            }
        }
        self.rounds.retain(|k, _| wants.iter().any(|w| w.key == k));
        out
    }

    pub fn succeeded(&mut self, key: &str) {
        self.rounds.remove(key);
    }

    /// No more automatic attempts until [`Reconnect::release`].
    pub fn hold(&mut self, key: &str, why: &'static str) {
        self.rounds.remove(key);
        self.held.insert(key.to_string(), why);
    }

    pub fn release(&mut self, key: &str) {
        self.held.remove(key);
    }

    pub fn held(&self, key: &str) -> Option<&'static str> {
        self.held.get(key).copied()
    }

    /// This round's tries are used up; it waits for the next trigger.
    pub fn gave_up(&self, key: &str) -> bool {
        self.rounds.get(key).is_some_and(|r| r.attempts >= BACKOFF.len())
    }

    pub fn active(&self, key: &str) -> bool {
        self.rounds.get(key).is_some_and(|r| r.attempts < BACKOFF.len())
    }
}

/// Wall-clock time passed but monotonic time didn't: the computer slept.
/// (Monotonic clocks pause in suspend on Linux and macOS; the second test
/// catches a long gap between polls that only take seconds anywhere.)
pub fn slept(wall_gap: Duration, mono_gap: Duration) -> bool {
    wall_gap > mono_gap + Duration::from_secs(20) || wall_gap > Duration::from_secs(120)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(success: bool, stdout: &str, stderr: &str) -> Output {
        Output { success, code: Some(if success { 0 } else { 2 }), stdout: stdout.into(), stderr: stderr.into() }
    }

    fn tools(os: Os) -> Tools {
        Tools::with(os, false, None)
    }

    #[test]
    fn names_are_validated_before_anything_runs() {
        assert!(ShareRef::new("100.64.0.1", "Home", "").is_ok());
        assert!(ShareRef::new("obsidian2.tail1234.ts.net", "Photo Archive", "nick").is_ok());
        assert!(ShareRef::new("fd7a:115c:a1e0::1", "Home", "").is_ok());
        for host in ["", "-oProxyCommand=x", "a b", "host;rm", "../x", "smb://x"] {
            assert!(ShareRef::new(host, "Home", "").is_err(), "{host}");
        }
        for share in ["", "-x", "a/b", "a\\b", "a;b", "a|b", "a\nb", "x*"] {
            assert!(ShareRef::new("h", share, "").is_err(), "{share:?}");
        }
        for user in ["-x", "a b", "a:b", "a/b", "a%b"] {
            assert!(ShareRef::new("h", "s", user).is_err(), "{user}");
        }
        // Slashes around a pasted share name are dropped, not rejected.
        assert_eq!(ShareRef::new(" h ", "/Home/", " ").unwrap(), ShareRef { host: "h".into(), share: "Home".into(), user: "".into() });
    }

    #[test]
    fn urls_and_unc_paths() {
        let s = ShareRef::new("100.106.250.76", "Home", "nick").unwrap();
        assert_eq!(s.url(), "smb://nick@100.106.250.76/Home");
        assert_eq!(s.unc(), r"\\100.106.250.76\Home");
        let s = ShareRef::new("box", "Photo Archive", "").unwrap();
        assert_eq!(s.url(), "smb://box/Photo%20Archive");
        assert_eq!(s.unc(), r"\\box\Photo Archive");
        let s = ShareRef::new("fd7a:115c:a1e0::1", "Home", "me@corp").unwrap();
        assert_eq!(s.url(), "smb://me%40corp@[fd7a:115c:a1e0::1]/Home");
        assert_eq!(s.unc(), r"\\fd7a-115c-a1e0--1.ipv6-literal.net\Home");
        assert_eq!(s.key(), "fd7a:115c:a1e0::1/home");
        assert_eq!(decode("Photo%20Archive"), "Photo Archive");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn reads_typed_locations() {
        let want = ShareRef::new("100.106.250.76", "Home", "nick").unwrap();
        assert_eq!(parse_location("smb://nick@100.106.250.76/Home").unwrap(), want);
        assert_eq!(parse_location("//nick@100.106.250.76/Home/").unwrap(), want);
        assert_eq!(parse_location(r"\\box\Photo Archive").unwrap(), ShareRef::new("box", "Photo Archive", "").unwrap());
        assert_eq!(parse_location("smb://box/Photo%20Archive").unwrap().share, "Photo Archive");
        assert!(parse_location("box").is_err());
        assert!(parse_location("//box/a/b").is_err());
        assert!(parse_location("//-x/Home").is_err());
    }

    #[test]
    fn each_os_connects_its_own_way() {
        let s = ShareRef::new("100.106.250.76", "Home", "nick").unwrap();
        let c = tools(Os::Mac).connect_cmd(&s);
        assert_eq!(c.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(c.args, ["-g", "smb://nick@100.106.250.76/Home"]);
        assert_eq!(tools(Os::Linux).connect_cmd(&s).args, ["mount", "smb://nick@100.106.250.76/Home"]);
        let c = tools(Os::Windows).connect_cmd(&s);
        assert!(c.program.to_string_lossy().to_lowercase().ends_with("net.exe"));
        assert_eq!(c.args, ["use", "*", r"\\100.106.250.76\Home", "/persistent:no"]);
        // No password ever goes on a command line, nor a /user that would make net use prompt.
        for os in [Os::Linux, Os::Mac, Os::Windows] {
            let t = tools(os);
            for c in [t.connect_cmd(&s), t.sign_in_cmd(&s)].iter().chain(t.list_cmds("h", "nick").iter()) {
                assert!(!c.args.iter().any(|a| a.to_lowercase().contains("pass") || a.starts_with("/user")), "{c:?}");
            }
        }
        assert_eq!(tools(Os::Windows).sign_in_cmd(&s).args, [r"\\100.106.250.76\Home"]);
        assert_eq!(tools(Os::Linux).sign_in_cmd(&s).args, ["smb://nick@100.106.250.76/Home"]);
        assert_eq!(tools(Os::Mac).sign_in_cmd(&s).args, ["smb://nick@100.106.250.76/Home"]);
    }

    #[test]
    fn listing_never_prompts() {
        let l = tools(Os::Linux).list_cmds("100.64.0.1", "");
        assert_eq!(l[0].args, ["-L", "//100.64.0.1", "-N", "-g", "-t", "8"]);
        assert_eq!(l[1].args, ["list", "smb://100.64.0.1/"]);
        assert_eq!(tools(Os::Mac).list_cmds("100.64.0.1", "nick")[0].args, ["view", "-N", "//nick@100.64.0.1"]);
        assert_eq!(tools(Os::Windows).list_cmds("100.64.0.1", "")[0].args, ["view", r"\\100.64.0.1"]);
    }

    #[test]
    fn disconnect_only_what_tailgate_could_have_made() {
        let gvfs = Mounted { host: "h".into(), share: "home".into(), user: "nick".into(), location: String::new(), kind: MountKind::Gvfs };
        assert_eq!(tools(Os::Linux).disconnect_cmd(&gvfs).unwrap().args, ["mount", "-u", "smb://nick@h/home"]);
        assert_eq!(tools(Os::Linux).open_cmd(&gvfs).args, ["smb://nick@h/home"]);
        let cifs = Mounted { kind: MountKind::Cifs, location: "/mnt/x".into(), ..gvfs.clone() };
        assert_eq!(tools(Os::Linux).disconnect_cmd(&cifs), None);
        let mac = Mounted { kind: MountKind::Smbfs, location: "/Volumes/Home".into(), ..gvfs.clone() };
        assert_eq!(tools(Os::Mac).disconnect_cmd(&mac).unwrap().args, ["unmount", "/Volumes/Home"]);
        assert_eq!(tools(Os::Mac).open_cmd(&mac).args, ["/Volumes/Home"]);
        let win = Mounted { kind: MountKind::NetUse, location: "Z:".into(), ..gvfs.clone() };
        assert_eq!(tools(Os::Windows).disconnect_cmd(&win).unwrap().args, ["use", "Z:", "/delete", "/y"]);
        assert_eq!(tools(Os::Windows).open_cmd(&win).args, [r"Z:\"]);
    }

    #[test]
    fn parses_smbclient_listing() {
        let text = "Disk|Pictures|\nDisk|Home|Home of nick\nIPC|IPC$|IPC Service (obsidian2)\nDisk|print$|Drivers\nPrinter|hp|\n";
        assert_eq!(parse_smbclient(text), ["Home", "Pictures"]);
    }

    #[test]
    fn parses_gio_listing() {
        assert_eq!(parse_gio_list("pictures\nhome\nIPC$\nphoto%20archive\n"), ["home", "photo archive", "pictures"]);
    }

    #[test]
    fn parses_smbutil_view() {
        let text = "Share                                           Type    Comments\n\
                    -------------------------------\n\
                    Photo Archive                                   Disk    \n\
                    Home                                            Disk    \n\
                    IPC$                                            Pipe    IPC Service (Samba 4.25.0)\n\
                    \n3 shares listed\n";
        assert_eq!(parse_share_table(text), ["Home", "Photo Archive"]);
    }

    #[test]
    fn parses_net_view() {
        let text = "Shared resources at \\\\100.86.43.122\r\n\r\n\r\n\r\nShare name  Type  Used as  Comment\r\n\r\n\
                    -------------------------------------------------------------------------------\r\n\
                    Users       Disk\r\nScans       Disk           Scanner drop\r\nPrinter1    Print\r\n\
                    A-very-long-share-name Disk\r\nThe command completed successfully.\r\n";
        assert_eq!(parse_share_table(text), ["A-very-long-share-name", "Scans", "Users"]);
        assert!(parse_share_table("System error 5 has occurred.\r\n\r\nAccess is denied.\r\n").is_empty());
    }

    #[test]
    fn parses_gio_mounts() {
        let text = "Drive(0): Samsung SSD\n  Type: GProxyDrive\nMount(0): home on 100.106.250.76 -> smb://nick@100.106.250.76/home/\n  Type: GDaemonMount\nMount(1): x -> file:///media/x\n";
        let fuse = vec![("100.106.250.76".to_string(), "home".to_string(), PathBuf::from("/run/user/1000/gvfs/smb-share:server=100.106.250.76,share=home,user=nick"))];
        let m = parse_gio_mounts(text, &fuse);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].host.as_str(), m[0].share.as_str(), m[0].user.as_str()), ("100.106.250.76", "home", "nick"));
        assert!(m[0].location.ends_with("share=home,user=nick"));
        assert!(m[0].is("Home", &["100.106.250.76".into()]));
        assert!(!m[0].is("Pictures", &["100.106.250.76".into()]));
        assert!(!m[0].is("home", &["100.106.250.77".into()]));
        let m = parse_gio_mounts("Mount(0): p on box -> smb://WORKGROUP;pat@box/photo%20archive/\n", &[]);
        assert_eq!((m[0].user.as_str(), m[0].host.as_str(), m[0].share.as_str(), m[0].location.as_str()), ("pat", "box", "photo archive", ""));
        assert_eq!(
            parse_gvfs_folder("smb-share:server=100.106.250.76,share=photo%20archive,user=nick"),
            Some(("100.106.250.76".into(), "photo archive".into()))
        );
        assert_eq!(parse_gvfs_folder("sftp:host=x"), None);
    }

    #[test]
    fn parses_proc_mounts() {
        let text = "proc /proc proc rw 0 0\n//100.64.0.9/Photo\\040Archive /mnt/photo\\040archive cifs rw,vers=3.1.1 0 0\n//nas/media/sub /mnt/m smb3 rw 0 0\n";
        let m = parse_proc_mounts(text);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].share.as_str(), m[0].location.as_str()), ("Photo Archive", "/mnt/photo archive"));
        assert_eq!((m[1].host.as_str(), m[1].share.as_str()), ("nas", "media"));
    }

    #[test]
    fn parses_mac_mounts() {
        let text = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                    //nick@100.106.250.76/Home on /Volumes/Home (smbfs, nodev, nosuid, mounted by ns)\n\
                    //GUEST:@box/Photo%20Archive on /Volumes/Photo Archive (smbfs, nodev, nosuid, mounted by ns)\n";
        let m = parse_mac_mounts(text);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], Mounted { host: "100.106.250.76".into(), share: "Home".into(), user: "nick".into(), location: "/Volumes/Home".into(), kind: MountKind::Smbfs });
        assert_eq!((m[1].user.as_str(), m[1].host.as_str(), m[1].share.as_str(), m[1].location.as_str()), ("GUEST", "box", "Photo Archive", "/Volumes/Photo Archive"));
    }

    #[test]
    fn parses_net_use() {
        let text = "New connections will not be remembered.\r\n\r\n\r\nStatus       Local     Remote                    Network\r\n\r\n\
                    -------------------------------------------------------------------------------\r\n\
                    OK           Z:        \\\\100.106.250.76\\Home     Microsoft Windows Network\r\n\
                    Unavailable  Y:        \\\\100.106.250.76\\Pictures Microsoft Windows Network\r\n\
                    OK                     \\\\box\\IPC$               Microsoft Windows Network\r\n\
                    OK                     \\\\box\\Scans              Microsoft Windows Network\r\n\
                    The command completed successfully.\r\n";
        let m = parse_net_use(text);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].location.as_str(), m[0].host.as_str(), m[0].share.as_str()), ("Z:", "100.106.250.76", "Home"));
        assert_eq!((m[1].location.as_str(), m[1].share.as_str()), ("", "Scans"));
        let pics = ShareRef::new("100.106.250.76", "pictures", "").unwrap();
        assert_eq!(stale_drives(text, &pics), ["Y:"]);
        assert!(stale_drives(text, &ShareRef::new("100.106.250.76", "Home", "").unwrap()).is_empty());
    }

    #[test]
    fn classifies_connect_results() {
        assert_eq!(classify(Os::Linux, &out(true, "", "")), Outcome::Connected("".into()));
        assert_eq!(classify(Os::Linux, &out(false, "", "gio: smb://h/home/: Location is already mounted")), Outcome::Connected("".into()));
        assert!(matches!(classify(Os::Linux, &out(false, "Password required for share home on h\nPassword: ", "gio: smb://h/home: Invalid argument")), Outcome::NeedsSignIn(_)));
        assert!(matches!(classify(Os::Linux, &out(false, "", "gio: smb://h/home: The specified location is not supported")), Outcome::Unsupported(_)));
        assert!(matches!(classify(Os::Linux, &out(false, "", "gio: smb://h/home: Failed to mount Windows share: Connection timed out")), Outcome::Failed(_)));
        assert_eq!(classify(Os::Mac, &out(true, "", "")), Outcome::Requested);
        assert_eq!(classify(Os::Windows, &out(true, "Drive Z: is now connected to \\\\h\\Home.\r\n\r\nThe command completed successfully.\r\n", "")), Outcome::Connected("Z:".into()));
        assert!(matches!(classify(Os::Windows, &out(false, "", "System error 1326 has occurred.\r\n\r\nThe user name or password is incorrect.\r\n")), Outcome::NeedsSignIn(_)));
        assert!(matches!(classify(Os::Windows, &out(false, "", "Systemfehler 86 aufgetreten.\r\n")), Outcome::NeedsSignIn(_)));
        // "86" inside an IP isn't error 86.
        assert!(matches!(classify(Os::Windows, &out(false, "", "System error 53 has occurred. \\\\100.86.43.122 not found\r\n")), Outcome::Failed(_)));
        assert!(matches!(classify(Os::Windows, &out(false, "", "System error 1219 has occurred.")), Outcome::Failed(m) if m.contains("different user")));
    }

    #[test]
    fn read_only_refuses_before_running() {
        let t = Tools::with(Os::Linux, true, Some(PathBuf::from("/nonexistent")));
        let s = ShareRef::new("h", "s", "").unwrap();
        let r = connect(&t, &s, true);
        assert!(matches!(r.outcome, Outcome::Failed(ref m) if m.starts_with("Read-only")) && !r.handed_off);
        let m = Mounted { host: "h".into(), share: "s".into(), user: "".into(), location: "".into(), kind: MountKind::Gvfs };
        assert!(disconnect(&t, &m).unwrap_err().starts_with("Read-only"));
    }

    #[test]
    fn reconnect_waits_for_a_trigger_then_backs_off_and_stops() {
        let t0 = Instant::now();
        let mut r = Reconnect::default();
        let want = |connected, reachable| [Want { key: "h/home", connected, reachable }];
        // Nothing happens without a trigger, however often it's asked.
        assert!(r.due(t0, &want(false, true)).is_empty());
        r.trigger(t0, ["h/home"]);
        // Offline host: no attempt, and no try used up.
        assert!(r.due(t0, &want(false, false)).is_empty());
        let mut at = t0;
        let mut tries = Vec::new();
        for _ in 0..20000 {
            at += Duration::from_secs(1);
            if !r.due(at, &want(false, true)).is_empty() {
                tries.push(at.duration_since(t0).as_secs());
            }
        }
        // Right away, then 5 s, 20 s, 1 min, 3 min and 10 min later; then it stops.
        assert_eq!(tries, [1, 6, 26, 86, 266, 866]);
        assert!(r.gave_up("h/home"));
        // Tailscale coming back starts a new round.
        r.trigger(at, ["h/home"]);
        assert_eq!(r.due(at, &want(false, true)), ["h/home"]);
        // Connected ends the round.
        assert!(r.due(at + Duration::from_secs(60), &want(true, true)).is_empty());
        assert!(!r.active("h/home"));
    }

    #[test]
    fn held_shares_are_left_alone_until_released() {
        let t0 = Instant::now();
        let mut r = Reconnect::default();
        let want = [Want { key: "h/home", connected: false, reachable: true }];
        r.hold("h/home", "needs sign-in");
        r.trigger(t0, ["h/home"]);
        assert!(r.due(t0, &want).is_empty());
        assert_eq!(r.held("h/home"), Some("needs sign-in"));
        r.release("h/home");
        r.trigger(t0, ["h/home"]);
        assert_eq!(r.due(t0, &want), ["h/home"]);
        // A share that isn't wanted any more is forgotten.
        r.trigger(t0, ["h/x"]);
        r.due(t0, &[]);
        assert!(!r.active("h/x"));
    }

    #[test]
    fn notices_sleep() {
        let s = Duration::from_secs;
        assert!(!slept(s(4), s(4)));
        assert!(!slept(s(30), s(28))); // a slow poll, not sleep
        assert!(slept(s(600), s(4)));
        assert!(slept(s(130), s(130)));
    }
}
