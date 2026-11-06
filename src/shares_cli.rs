//! `--shares`, `--connect-share` and `--disconnect-share`: the Shares tab's
//! plumbing from a terminal, for checking a setup (or Tailgate) without the window.

use std::time::{Duration, Instant};

use tailgate::cli::{Client, Query};
use tailgate::shares::{self, Outcome, Tools};
use tailgate::status;

/// Scans the tailnet and prints what it finds. Changes nothing.
pub fn list() -> i32 {
    let client = Client::from_env(true);
    let tools = Tools::system(true);
    let status = match client.read(Query::Status).and_then(|t| status::parse_status(&t).map_err(tailgate::cli::CliError::Malformed)) {
        Ok(s) => s,
        Err(e) => {
            println!("Tailscale didn't answer: {e}");
            return 1;
        }
    };
    let candidates: Vec<shares::Candidate> = status
        .peers
        .iter()
        .filter(|p| p.online && !p.mullvad)
        .filter_map(|p| Some(shares::Candidate { host: p.ipv4()?.to_string(), name: p.display_name() }))
        .collect();
    println!("Probing port 445 on {} online devices…", candidates.len());
    let mut found = shares::scan(&tools, &candidates);
    found.sort_by_key(|f| f.name.to_lowercase());
    if found.is_empty() {
        println!("  none answered");
    }
    for f in &found {
        match &f.shares {
            Some(list) => println!("  {} ({}): {}", f.name, f.host, list.join(", ")),
            None => println!("  {} ({}): file sharing answers; listing its shares needs a sign-in", f.name, f.host),
        }
    }
    let mounted = shares::list_mounted(&tools);
    println!("Connected now:");
    if mounted.is_empty() {
        println!("  nothing");
    }
    for m in mounted {
        let user = if m.user.is_empty() { String::new() } else { format!("{}@", m.user) };
        let at = if m.location.is_empty() { String::new() } else { format!(" at {}", m.location) };
        println!("  //{user}{}/{} ({:?}){at}", m.host, m.share, m.kind);
    }
    0
}

pub fn connect(target: &str, connect: bool, read_only: bool) -> i32 {
    let tools = Tools::system(read_only);
    let share = match shares::parse_location(target) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tailgate: {e}");
            return 2;
        }
    };
    let find = || shares::list_mounted(&tools).into_iter().find(|m| m.is(&share.share, std::slice::from_ref(&share.host)));
    if !connect {
        let Some(m) = find() else {
            println!("{} isn't connected.", share.unc());
            return 1;
        };
        return match shares::disconnect(&tools, &m) {
            Ok(()) => {
                println!("Disconnected {}.", share.share);
                0
            }
            Err(e) => {
                println!("Couldn't disconnect: {e}");
                1
            }
        };
    }
    if let Some(m) = find() {
        println!("Already connected{}.", if m.location.is_empty() { String::new() } else { format!(" at {}", m.location) });
        return 0;
    }
    let result = shares::connect(&tools, &share, false);
    match result.outcome {
        Outcome::Connected(_) | Outcome::Requested => {
            // Finder mounts in its own time; give it a moment to show up.
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(30) {
                if let Some(m) = find() {
                    println!("Connected{}.", if m.location.is_empty() { String::new() } else { format!(" at {}", m.location) });
                    return 0;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            println!("Asked to connect, but it hasn't shown up yet (a password prompt may be waiting).");
            1
        }
        Outcome::NeedsSignIn(msg) => {
            println!("Needs a sign-in that {} doesn't have: {msg}", tools.os.keychain());
            1
        }
        Outcome::Unsupported(msg) => {
            println!("This system can't mount SMB: {msg}");
            1
        }
        Outcome::Failed(msg) => {
            println!("Couldn't connect: {msg}");
            1
        }
    }
}
