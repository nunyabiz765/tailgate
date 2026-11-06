//! Runs the real process plumbing against `tests/fake-tailscale`, which records
//! its argv. Nothing here can reach the real daemon.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tailgate::cli::{Action, CliError, Client, Query};
use tailgate::status;

static NEXT: AtomicU32 = AtomicU32::new(0);

/// Writes an executable script and waits until it can run. Another test thread
/// forking at the wrong moment can briefly hold our write handle, which makes
/// exec fail with "text file busy".
#[cfg(unix)]
fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, format!("#!/bin/sh\n[ \"$1\" = __probe ] && exit 0\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..200 {
        match std::process::Command::new(path).arg("__probe").status() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(Duration::from_millis(5)),
            _ => return,
        }
    }
    panic!("{} stayed busy", path.display());
}

/// The fake as a script for this OS: `sh` on Unix, a `.cmd` running Python on Windows.
#[cfg(unix)]
fn fake_tailscale(dir: &Path, mode: &str) -> PathBuf {
    let wrapper = dir.join("tailscale");
    write_script(
        &wrapper,
        &format!(
            "FAKE_TS_LOG='{log}' FAKE_TS_STATE='{state}' FAKE_TS_MODE='{mode}' exec '{root}/tests/fake-tailscale' \"$@\"",
            log = dir.join("log").display(),
            state = dir.join("state").display(),
            root = env!("CARGO_MANIFEST_DIR"),
        ),
    );
    wrapper
}

#[cfg(windows)]
fn fake_tailscale(dir: &Path, mode: &str) -> PathBuf {
    let wrapper = dir.join("tailscale.cmd");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::write(
        &wrapper,
        format!(
            "@echo off\r\nset \"FAKE_TS_LOG={log}\"\r\nset \"FAKE_TS_STATE={state}\"\r\nset \"FAKE_TS_MODE={mode}\"\r\n\
             set PYTHONUTF8=1\r\npython \"{script}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
            log = dir.join("log").display(),
            state = dir.join("state").display(),
            script = root.join("tests").join("fake-tailscale").display(),
        ),
    )
    .unwrap();
    wrapper
}

/// A fake binary wrapper with its own log, so tests can run in parallel. The
/// wrapper script is how a test passes env to the child without touching this
/// process's environment.
struct Fake {
    dir: PathBuf,
    client: Client,
}

impl Fake {
    fn new(mode: &str) -> Fake {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tailgate-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let wrapper = fake_tailscale(&dir, mode);
        let mut client = Client::with_bin(&wrapper);
        // On Windows the fake is Python behind a .cmd, and a cold start on a CI runner
        // (antivirus scanning python.exe) can take longer than the app's 8 s read deadline.
        #[cfg(windows)]
        {
            client.read_timeout = Duration::from_secs(45);
        }
        #[cfg(unix)]
        {
            let pkexec = dir.join("pkexec");
            write_script(
                &pkexec,
                &format!(
                    "FAKE_PKEXEC_LOG='{}' exec '{}/tests/fake-pkexec' \"$@\"",
                    dir.join("pkexec.log").display(),
                    env!("CARGO_MANIFEST_DIR")
                ),
            );
            client.pkexec = pkexec;
            client.custom_pkexec = true;
        }
        Fake { dir, client }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        let Ok(text) = fs::read_to_string(self.dir.join("log")) else { return Vec::new() };
        text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    fn pkexec_log(&self) -> String {
        fs::read_to_string(self.dir.join("pkexec.log")).unwrap_or_default()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stockholm() -> Action {
    Action::SetExitNode { ip: "100.96.1.31".into(), label: "Stockholm".into() }
}

fn first_node_ip(fake: &Fake) -> String {
    let text = fake.client.read(Query::Status).unwrap();
    let parsed = status::parse_status(&text).unwrap();
    let node = parsed.exit_nodes().find(|p| p.mullvad).unwrap();
    node.ipv4().unwrap().to_string()
}

#[test]
fn reads_parse_end_to_end_and_only_read_commands_run() {
    let fake = Fake::new("ok");
    let st = status::parse_status(&fake.client.read(Query::Status).unwrap()).unwrap();
    assert!(st.exit_nodes().count() > 100);
    assert_eq!(st.current_exit_peer().unwrap().short_name(), "se-sto-wg-001");
    let prefs = status::parse_prefs(&fake.client.read(Query::Prefs).unwrap()).unwrap();
    assert!(prefs.allow_lan);
    let serve = status::parse_serve(&fake.client.read(Query::ServeJson).unwrap()).unwrap();
    assert_eq!(serve.len(), 3);
    assert!(fake.client.read(Query::ServeText).unwrap().contains("tailnet only"));
    assert_eq!(
        fake.calls(),
        [
            vec!["status", "--json"],
            vec!["debug", "prefs"],
            vec!["serve", "status", "--json"],
            vec!["serve", "status"],
        ]
    );
}

#[test]
fn mutations_send_exactly_the_expected_argv_and_take_effect() {
    let fake = Fake::new("ok");
    let ip = first_node_ip(&fake);
    let action = Action::SetExitNode { ip: ip.clone(), label: "x".into() };
    fake.client.mutate(&action, false).unwrap();
    fake.client.mutate(&Action::ClearExitNode, false).unwrap();
    fake.client.mutate(&Action::AllowLan(false), false).unwrap();
    fake.client.mutate(&Action::Down, false).unwrap();
    fake.client.mutate(&Action::Up, false).unwrap();
    let calls = fake.calls();
    assert_eq!(calls[1..], [
        vec!["set".to_string(), format!("--exit-node={ip}")],
        vec!["set".to_string(), "--exit-node=".to_string()],
        vec!["set".to_string(), "--exit-node-allow-lan-access=false".to_string()],
        vec!["down".to_string()],
        vec!["up".to_string()],
    ]);
    let prefs = status::parse_prefs(&fake.client.read(Query::Prefs).unwrap()).unwrap();
    assert!(!prefs.has_exit_node() && !prefs.allow_lan && prefs.want_running);
}

#[test]
fn down_then_status_reports_stopped_with_no_peers() {
    let fake = Fake::new("ok");
    fake.client.mutate(&Action::Down, false).unwrap();
    let st = status::parse_status(&fake.client.read(Query::Status).unwrap()).unwrap();
    assert_eq!(st.backend, status::Backend::Stopped);
    assert!(st.peers.is_empty());
}

#[test]
fn read_only_mode_never_spawns_anything() {
    let mut fake = Fake::new("ok");
    fake.client.read_only = true;
    for action in [stockholm(), Action::ClearExitNode, Action::AllowLan(true), Action::Up, Action::Down, Action::SetOperator("pat".into())] {
        for elevate in [false, true] {
            assert!(matches!(fake.client.mutate(&action, elevate), Err(CliError::ReadOnly(_))));
        }
    }
    assert!(fake.calls().is_empty());
    assert!(fake.pkexec_log().is_empty());
    // Reads still work.
    assert!(fake.client.read(Query::Status).is_ok());
}

#[test]
fn missing_cli_is_reported() {
    let client = Client::with_bin("/nonexistent/tailscale");
    assert!(matches!(client.read(Query::Status), Err(CliError::Missing(_))));
    assert!(matches!(client.mutate(&Action::Up, false), Err(CliError::Missing(_))));
}

#[test]
fn access_denied_is_classified() {
    let fake = Fake::new("denied");
    match fake.client.mutate(&Action::ClearExitNode, false) {
        Err(CliError::AccessDenied(msg)) => assert!(msg.contains("checkprefs")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn slow_cli_times_out_and_kills_the_whole_group() {
    let mut fake = Fake::new("slow");
    fake.client.read_timeout = Duration::from_millis(300);
    fake.client.write_timeout = Some(Duration::from_millis(300));
    let started = Instant::now();
    assert!(matches!(fake.client.read(Query::Status), Err(CliError::Timeout(_))));
    assert!(matches!(fake.client.mutate(&Action::Down, false), Err(CliError::Timeout(_))));
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
}

#[test]
fn node_that_has_gone_is_a_plain_failure_with_the_cli_message() {
    let fake = Fake::new("gone");
    match fake.client.mutate(&stockholm(), false) {
        Err(CliError::Failed(msg)) => assert!(msg.contains("no node found")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn malformed_output_is_an_error_not_a_panic() {
    let fake = Fake::new("malformed");
    let text = fake.client.read(Query::Status).unwrap();
    assert!(status::parse_status(&text).is_err());
    let text = fake.client.read(Query::Prefs).unwrap();
    assert!(status::parse_prefs(&text).is_err());
}

#[test]
fn daemon_down_surfaces_the_cli_message() {
    let fake = Fake::new("fail");
    match fake.client.read(Query::Status) {
        Err(CliError::Failed(msg)) => assert!(msg.contains("tailscaled")),
        other => panic!("{other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn operator_grant_goes_through_pkexec_and_unlocks_later_changes() {
    let fake = Fake::new("denied-until-operator");
    assert!(matches!(fake.client.mutate(&Action::ClearExitNode, false), Err(CliError::AccessDenied(_))));
    fake.client.mutate(&Action::SetOperator("pat".into()), true).unwrap();
    assert_eq!(fake.pkexec_log().trim(), format!("{} set --operator=pat", fake.client.bin.display()));
    fake.client.mutate(&Action::ClearExitNode, false).unwrap();
    let prefs = status::parse_prefs(&fake.client.read(Query::Prefs).unwrap()).unwrap();
    assert_eq!(prefs.operator.as_deref(), Some("pat"));
    assert!(!prefs.has_exit_node());
}

#[cfg(unix)]
#[test]
fn just_this_once_runs_one_change_elevated_and_grants_nothing() {
    let fake = Fake::new("denied");
    fake.client.mutate(&Action::ClearExitNode, true).unwrap();
    assert!(fake.pkexec_log().contains("set --exit-node="));
    let prefs = status::parse_prefs(&fake.client.read(Query::Prefs).unwrap()).unwrap();
    assert!(prefs.operator.is_none());
    assert!(matches!(fake.client.mutate(&Action::AllowLan(false), false), Err(CliError::AccessDenied(_))));
}

#[cfg(unix)]
#[test]
fn dismissed_password_prompt_is_a_clean_decline() {
    let fake = Fake::new("denied");
    for code in ["dismissed", "refused"] {
        let wrapper = fake.dir.join(format!("pkexec-{code}"));
        write_script(
            &wrapper,
            &format!(
                "FAKE_PKEXEC_MODE={code} FAKE_PKEXEC_LOG='{}' exec '{}/tests/fake-pkexec' \"$@\"",
                fake.dir.join("pkexec.log").display(),
                env!("CARGO_MANIFEST_DIR"),
            ),
        );
        let mut client = fake.client.clone();
        client.pkexec = wrapper;
        assert_eq!(client.mutate(&Action::ClearExitNode, true), Err(CliError::AuthDeclined));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_binary_is_never_run_through_the_real_pkexec() {
    let fake = Fake::new("ok");
    let mut client = fake.client.clone();
    client.custom_pkexec = false;
    client.pkexec = PathBuf::from("/nonexistent/should-not-run");
    assert_eq!(client.mutate(&Action::Up, true), Err(CliError::ElevationRefused));
    assert!(fake.calls().is_empty());
}

/// Windows and macOS have no pkexec, so an elevated change is refused before
/// anything runs, whatever the binary.
#[cfg(not(target_os = "linux"))]
#[test]
fn no_elevation_outside_linux() {
    let fake = Fake::new("ok");
    let mut client = fake.client.clone();
    client.custom_pkexec = false;
    assert_eq!(client.mutate(&Action::Up, true), Err(CliError::ElevationUnavailable));
    assert!(fake.calls().is_empty());
}

/// A wrapper for tests/fake-install-tailscale that "installs" the fake CLI at
/// `target`, the binary `client` was pointed at before it existed.
#[cfg(unix)]
fn fake_installer(fake: &Fake, mode: &str) -> (PathBuf, Client) {
    let target = fake.dir.join("installed-tailscale");
    let helper = fake.dir.join(format!("install-{mode}"));
    write_script(
        &helper,
        &format!(
            "FAKE_INSTALL_MODE={mode} FAKE_INSTALL_TARGET='{target}' FAKE_TS_LOG='{log}' FAKE_TS_STATE='{state}' FAKE_TS_MODE=login \
             exec '{root}/tests/fake-install-tailscale' \"$@\"",
            target = target.display(),
            log = fake.dir.join("log").display(),
            state = fake.dir.join("state").display(),
            root = env!("CARGO_MANIFEST_DIR"),
        ),
    );
    let mut client = fake.client.clone();
    client.bin = target;
    (helper, client)
}

#[cfg(unix)]
#[test]
fn one_click_install_reports_each_step_and_makes_the_caller_operator() {
    let fake = Fake::new("login");
    let (helper, client) = fake_installer(&fake, "ok");
    assert!(matches!(client.read(Query::Status), Err(CliError::Missing(_))));
    let mut steps = Vec::new();
    client.install_tailscale(&helper, &mut |s| steps.push(s.to_string())).unwrap();
    assert_eq!(fake.pkexec_log().trim(), helper.display().to_string(), "helper runs through pkexec with no arguments");
    assert_eq!(steps.first().map(String::as_str), Some("Adding Tailscale's software source…"));
    assert_eq!(steps.last().map(String::as_str), Some("Done."));
    assert!(steps.iter().any(|s| s.starts_with("Letting ")));
    let user = tailgate::cli::current_user().unwrap();
    let prefs = status::parse_prefs(&client.read(Query::Prefs).unwrap()).unwrap();
    assert_eq!(prefs.operator.as_deref(), Some(user.as_str()));
}

#[cfg(unix)]
#[test]
fn one_click_install_failures_come_back_in_plain_words() {
    let fake = Fake::new("login");
    for (mode, want) in [
        ("offline", "Check that this computer is online"),
        ("busy", "Software Updater"),
        ("broken", "Installing Tailscale didn't finish. E: something odd"),
    ] {
        let (helper, client) = fake_installer(&fake, mode);
        match client.install_tailscale(&helper, &mut |_| {}) {
            Err(CliError::Failed(msg)) => {
                assert!(msg.contains(want), "{mode}: {msg}");
                assert!(msg.ends_with("(Details: /var/log/tailgate-install.log)"), "{msg}");
            }
            other => panic!("{mode}: {other:?}"),
        }
        assert!(matches!(client.read(Query::Status), Err(CliError::Missing(_))));
    }
}

#[cfg(unix)]
#[test]
fn one_click_install_password_cancelled_and_read_only() {
    let fake = Fake::new("login");
    let (helper, mut client) = fake_installer(&fake, "ok");
    let declined = fake.dir.join("pkexec-dismissed");
    write_script(
        &declined,
        &format!(
            "FAKE_PKEXEC_MODE=dismissed FAKE_PKEXEC_LOG='{}' exec '{}/tests/fake-pkexec' \"$@\"",
            fake.dir.join("pkexec.log").display(),
            env!("CARGO_MANIFEST_DIR"),
        ),
    );
    let real_pkexec = std::mem::replace(&mut client.pkexec, declined);
    assert_eq!(client.install_tailscale(&helper, &mut |_| {}), Err(CliError::AuthDeclined));
    client.pkexec = real_pkexec;
    client.read_only = true;
    assert!(matches!(client.install_tailscale(&helper, &mut |_| {}), Err(CliError::ReadOnly(_))));
    client.read_only = false;
    client.custom_pkexec = false;
    assert_eq!(client.install_tailscale(&helper, &mut |_| {}), Err(CliError::ElevationRefused));
    assert!(matches!(client.read(Query::Status), Err(CliError::Missing(_))));
}

/// The real helper's system detection, and its refusal to take arguments or run
/// unprivileged. (Installing for real is tested in CI's Ubuntu containers.)
#[cfg(target_os = "linux")]
#[test]
fn install_helper_detects_ubuntu_bases_and_refuses_misuse() {
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/linux/install-tailscale");
    let dir = std::env::temp_dir().join(format!("tailgate-osrel-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let cases = [
        ("zorin17", "NAME=\"Zorin OS\"\nID=zorin\nID_LIKE=\"ubuntu debian\"\nVERSION_CODENAME=jammy\nUBUNTU_CODENAME=jammy\n", "ubuntu jammy"),
        ("zorin18", "NAME=\"Zorin OS\"\nID=zorin\nID_LIKE=\"ubuntu debian\"\nVERSION_CODENAME=noble\nUBUNTU_CODENAME=noble\n", "ubuntu noble"),
        ("mint22", "NAME=\"Linux Mint\"\nID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_CODENAME=wilma\nUBUNTU_CODENAME=noble\n", "ubuntu noble"),
        ("ubuntu", "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_CODENAME=noble\nUBUNTU_CODENAME=noble\n", "ubuntu noble"),
        ("debian12", "NAME=\"Debian GNU/Linux\"\nID=debian\nVERSION_CODENAME=bookworm\n", "debian bookworm"),
        ("fedora", "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=41\n", "unsupported"),
    ];
    for (name, text, want) in cases {
        let file = dir.join(name);
        fs::write(&file, text).unwrap();
        let out = std::process::Command::new(&helper).arg("--detect").arg(&file).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), want, "{name}");
        assert_eq!(out.status.code(), Some(if want == "unsupported" { 22 } else { 0 }), "{name}");
    }
    let _ = fs::remove_dir_all(&dir);
    let out = std::process::Command::new(&helper).arg("--operator=evil").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    if unsafe { libc::geteuid() } != 0 {
        let out = std::process::Command::new(&helper).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "must refuse to run unprivileged");
    }
}
