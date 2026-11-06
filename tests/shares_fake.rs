//! Drives the Shares plumbing against `tests/fake-smb`, for all three OSes'
//! command sets, on Unix. Nothing here probes a real network or mounts anything.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tailgate::shares::{self, Candidate, MountKind, Os, Outcome, ShareRef, Tools};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Fake {
    dir: PathBuf,
    tools: Tools,
}

const NAMES: [&str; 9] = ["probe", "smbclient", "gio", "open", "mount", "diskutil", "net", "explorer", "xdg-open"];

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, format!("#!/bin/sh\n[ \"$1\" = __probe ] && exit 0\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    // Another test thread forking mid-write can make exec fail with "text file busy".
    for _ in 0..200 {
        match std::process::Command::new(path).arg("__probe").status() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(Duration::from_millis(5)),
            _ => return,
        }
    }
    panic!("{} stayed busy", path.display());
}

impl Fake {
    fn new(os: Os, mode: &str) -> Fake {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tailgate-shares-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in NAMES {
            write_script(
                &dir.join(name),
                &format!(
                    "FAKE_SMB_LOG='{d}/log' FAKE_SMB_STATE='{d}/state' FAKE_SMB_MODE='{mode}' \
                     FAKE_SMB_HOSTS='100.64.0.2 100.64.0.5' FAKE_SMB_LIST='100.64.0.5' FAKE_SMB_SHARES='Media Backups' \
                     exec '{root}/tests/fake-smb' {name} \"$@\"",
                    d = dir.display(),
                    root = env!("CARGO_MANIFEST_DIR"),
                ),
            );
        }
        let tools = Tools::with(os, false, Some(dir.clone()));
        Fake { dir, tools }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("log")).unwrap_or_default().lines().map(|l| l.replace('\t', " ")).collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn home() -> ShareRef {
    ShareRef::new("100.64.0.5", "Media", "pat").unwrap()
}

#[test]
fn scan_probes_then_lists_only_hosts_that_answer() {
    let fake = Fake::new(Os::Linux, "ok");
    let candidates = ["100.64.0.2", "100.64.0.3", "100.64.0.5"].map(|h| Candidate { host: h.into(), name: format!("dev-{h}") });
    let mut found = shares::scan(&fake.tools, &candidates);
    found.sort_by(|a, b| a.host.cmp(&b.host));
    assert_eq!(found.len(), 2);
    // Answers on 445 but won't list anonymously, and gvfs has no saved sign-in either.
    assert_eq!((found[0].host.as_str(), found[0].shares.clone()), ("100.64.0.2", None));
    assert_eq!(found[1].shares, Some(vec!["Backups".to_string(), "Media".to_string()]));
    let calls = fake.calls();
    assert!(!calls.iter().any(|c| c.starts_with("smbclient -L //100.64.0.3")), "no listing for a silent host: {calls:?}");
    assert!(calls.contains(&"gio list smb://100.64.0.2/".to_string()), "{calls:?}");
}

#[test]
fn linux_connects_with_gio_and_disconnects() {
    let fake = Fake::new(Os::Linux, "ok");
    let r = shares::connect(&fake.tools, &home(), true);
    assert_eq!(r.outcome, Outcome::Connected(String::new()));
    assert!(!r.handed_off, "no file manager when it just worked");
    let mounted = shares::list_mounted(&fake.tools);
    assert_eq!(mounted.len(), 1);
    assert!(mounted[0].is("media", &["100.64.0.5".into()]));
    assert_eq!(mounted[0].kind, MountKind::Gvfs);
    shares::disconnect(&fake.tools, &mounted[0]).unwrap();
    assert!(shares::list_mounted(&fake.tools).is_empty());
    let calls = fake.calls();
    assert!(calls.contains(&"gio mount smb://pat@100.64.0.5/Media".to_string()), "{calls:?}");
    assert!(calls.contains(&"gio mount -u smb://pat@100.64.0.5/Media".to_string()), "{calls:?}");
}

#[test]
fn missing_sign_in_goes_to_the_file_manager_only_when_clicked() {
    let fake = Fake::new(Os::Linux, "needs-password");
    // An automatic reconnect never pops anything up.
    let r = shares::connect(&fake.tools, &home(), false);
    assert!(matches!(r.outcome, Outcome::NeedsSignIn(_)) && !r.handed_off);
    assert!(!fake.calls().iter().any(|c| c.starts_with("xdg-open")));
    // A click hands it to the file manager, which asks in the desktop's own dialog.
    let r = shares::connect(&fake.tools, &home(), true);
    assert!(matches!(r.outcome, Outcome::NeedsSignIn(_)) && r.handed_off);
    assert!(fake.calls().contains(&"xdg-open smb://pat@100.64.0.5/Media".to_string()));
}

#[test]
fn no_gvfs_smb_falls_back_to_the_file_manager() {
    let fake = Fake::new(Os::Linux, "unsupported");
    let r = shares::connect(&fake.tools, &home(), true);
    assert!(matches!(r.outcome, Outcome::Unsupported(_)) && r.handed_off, "{r:?}");
}

#[test]
fn mac_hands_the_url_to_finder_and_sees_the_mount() {
    let fake = Fake::new(Os::Mac, "ok");
    let r = shares::connect(&fake.tools, &home(), true);
    assert_eq!(r.outcome, Outcome::Requested);
    let mounted = shares::list_mounted(&fake.tools);
    assert_eq!(mounted.len(), 1);
    assert_eq!((mounted[0].user.as_str(), mounted[0].location.as_str()), ("pat", "/Volumes/Media"));
    shares::disconnect(&fake.tools, &mounted[0]).unwrap();
    assert!(shares::list_mounted(&fake.tools).is_empty());
    let calls = fake.calls();
    assert!(calls.contains(&"open -g smb://pat@100.64.0.5/Media".to_string()), "{calls:?}");
    assert!(calls.contains(&"diskutil unmount /Volumes/Media".to_string()), "{calls:?}");
}

#[test]
fn windows_maps_a_drive_and_unmaps_it() {
    let fake = Fake::new(Os::Windows, "ok");
    let r = shares::connect(&fake.tools, &home(), true);
    assert_eq!(r.outcome, Outcome::Connected("Z:".into()));
    let mounted = shares::list_mounted(&fake.tools);
    assert_eq!((mounted.len(), mounted[0].location.as_str()), (1, "Z:"));
    shares::disconnect(&fake.tools, &mounted[0]).unwrap();
    assert!(shares::list_mounted(&fake.tools).is_empty());
    let calls = fake.calls();
    assert!(calls.contains(&r"net use * \\100.64.0.5\Media /persistent:no".to_string()), "{calls:?}");
    assert!(calls.contains(&"net use Z: /delete /y".to_string()), "{calls:?}");
    assert_eq!(shares::list_shares(&fake.tools, "100.64.0.5", ""), Some(vec!["Backups".into(), "Media".into()]));
}

#[test]
fn windows_without_a_saved_sign_in_opens_explorer() {
    let fake = Fake::new(Os::Windows, "needs-password");
    let r = shares::connect(&fake.tools, &home(), true);
    assert!(matches!(r.outcome, Outcome::NeedsSignIn(_)) && r.handed_off);
    assert!(fake.calls().contains(&r"explorer \\100.64.0.5\Media".to_string()));
}

#[test]
fn read_only_runs_nothing() {
    let mut fake = Fake::new(Os::Linux, "ok");
    fake.tools.read_only = true;
    let r = shares::connect(&fake.tools, &home(), true);
    assert!(matches!(r.outcome, Outcome::Failed(_)));
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}
