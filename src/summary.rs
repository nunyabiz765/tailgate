//! Wording that depends on current state: the headline, and what "down" cuts.

use crate::cli::CliError;
use crate::status::{service_name, Backend, Peer, Prefs, ServeEntry, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good = 0,
    Warn = 1,
    Off = 2,
    Trouble = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headline {
    pub tone: Tone,
    pub pill: String,
    pub title: String,
    pub detail: String,
}

fn place_line(peer: &Peer) -> String {
    let country = peer.location.as_ref().map(|l| l.country.as_str()).filter(|c| !c.is_empty());
    match country {
        Some(country) => format!("{country} · {}", peer.short_name()),
        None => peer.short_name(),
    }
}

/// `known` is the last peer list seen. A stopped daemon reports no peers, but
/// its prefs still name the exit node it will come back to.
pub fn headline(status: Option<&Status>, prefs: Option<&Prefs>, known: &[Peer], error: Option<&str>) -> Headline {
    let Some(status) = status else {
        return Headline {
            tone: Tone::Trouble,
            pill: "No status".into(),
            title: "Can't read Tailscale".into(),
            detail: error.unwrap_or("Waiting for the first answer from the CLI.").to_string(),
        };
    };
    match &status.backend {
        Backend::Running => match status.current_exit_peer() {
            Some(peer) if !peer.online => Headline {
                tone: Tone::Trouble,
                pill: "Exit node offline".into(),
                title: peer.place(),
                detail: format!("{} · selected but offline, so traffic may not get out", peer.short_name()),
            },
            Some(peer) => Headline {
                tone: Tone::Good,
                pill: if peer.mullvad { "Through Mullvad".into() } else { "Through exit node".into() },
                title: peer.place(),
                detail: place_line(peer),
            },
            None if status.current_exit.is_some() || prefs.is_some_and(Prefs::has_exit_node) => Headline {
                tone: Tone::Trouble,
                pill: "Exit node missing".into(),
                title: "Unknown exit node".into(),
                detail: "The selected exit node isn't available, so traffic may not get out.".into(),
            },
            None => Headline {
                tone: Tone::Warn,
                pill: "Home IP showing".into(),
                title: "No exit node".into(),
                detail: "Tailscale is up. Sites see your home IP.".into(),
            },
        },
        Backend::Stopped => {
            let resume = prefs
                .and_then(|p| p.exit_node_id.as_deref())
                .filter(|id| !id.is_empty())
                .and_then(|id| known.iter().find(|p| p.id == id));
            Headline {
                tone: Tone::Off,
                pill: "Off".into(),
                title: "Tailscale is off".into(),
                detail: match resume {
                    Some(peer) => format!("Sites see your home IP. Comes back through {}.", peer.place()),
                    None => "Sites see your home IP.".into(),
                },
            }
        }
        Backend::NeedsLogin => Headline {
            tone: Tone::Trouble,
            pill: "Login needed".into(),
            title: "Log in to Tailscale".into(),
            detail: "This machine isn't signed in to a tailnet.".into(),
        },
        Backend::NeedsMachineAuth => Headline {
            tone: Tone::Trouble,
            pill: "Approval needed".into(),
            title: "Waiting for approval".into(),
            detail: "An admin has to approve this machine in the console.".into(),
        },
        Backend::Starting => Headline {
            tone: Tone::Warn,
            pill: "Starting".into(),
            title: "Tailscale is starting".into(),
            detail: "Connecting to the tailnet.".into(),
        },
        Backend::Other(state) => Headline {
            tone: Tone::Trouble,
            pill: "Unknown state".into(),
            title: "Tailscale is in an unusual state".into(),
            detail: format!("The daemon reports \"{state}\"."),
        },
    }
}

/// One line per service found in serve status, with ports sharing a name together.
fn serve_lines(entries: &[ServeEntry]) -> Vec<String> {
    let mut groups: Vec<(&str, Vec<u32>, bool)> = Vec::new();
    for entry in entries {
        match groups.iter_mut().find(|(name, ..)| *name == entry.name) {
            Some((_, ports, public)) => {
                if !ports.contains(&entry.port) {
                    ports.push(entry.port);
                }
                *public |= entry.public;
            }
            None => groups.push((&entry.name, vec![entry.port], entry.public)),
        }
    }
    groups
        .into_iter()
        .map(|(name, ports, public)| {
            let reach = if public { "on the tailnet and the public internet" } else { "on the tailnet" };
            match ports.as_slice() {
                [one] if name == service_name(*one) => format!("{name}: stops answering {reach}."),
                [one] => format!("{name}: port {one} stops answering {reach}."),
                many => {
                    let list: Vec<String> = many.iter().map(u32::to_string).collect();
                    format!("{name}: ports {} stop answering {reach}.", list.join(" and "))
                }
            }
        })
        .collect()
}

/// One line per thing `tailscale down` cuts. `serve` is None if we couldn't read it,
/// and the dialog says so rather than guessing at what's there.
pub fn down_impact(status: Option<&Status>, serve: Option<&[ServeEntry]>) -> Vec<String> {
    let mut lines = Vec::new();

    match status.and_then(Status::current_exit_peer) {
        Some(peer) if peer.mullvad => {
            lines.push(format!("Mullvad: traffic stops going through {} and leaves on your home IP.", peer.place()))
        }
        Some(peer) => lines.push(format!(
            "Exit node: traffic stops going through {} and leaves on your home IP.",
            peer.place()
        )),
        None => {}
    }

    match serve {
        Some(entries) => lines.extend(serve_lines(entries)),
        None => lines.push(
            "Anything this machine serves over Tailscale stops answering. Tailgate couldn't read serve status, so it can't list what that includes.".into(),
        ),
    }

    let machine = status
        .map(|s| s.self_peer.display_name())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this machine".to_string());
    lines.push(format!(
        "Remote access: you can't reach {machine} over Tailscale, and can't turn it back on from elsewhere."
    ));
    lines
}

/// Which first-run step the Linux setup guide should show: 0 none (Tailscale
/// answered and is signed in), 1 install Tailscale, 2 start tailscaled, 3 sign in.
pub fn setup_step(status: Result<&Status, &CliError>) -> i32 {
    match status {
        Ok(status) if status.backend == Backend::NeedsLogin => 3,
        Ok(_) => 0,
        Err(CliError::Missing(_)) => 1,
        Err(CliError::Failed(message)) => {
            let m = message.to_lowercase();
            if m.contains("tailscaled") || m.contains("tailscale daemon") { 2 } else { 0 }
        }
        Err(_) => 0,
    }
}

/// Plain-English backend state for the "Tailscale" row.
pub fn is_up(status: Option<&Status>) -> bool {
    matches!(status.map(|s| &s.backend), Some(Backend::Running | Backend::Starting))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::parse_status;

    #[test]
    fn setup_step_follows_first_run() {
        let missing = CliError::Missing("tailscale".into());
        assert_eq!(setup_step(Err(&missing)), 1);
        let old = CliError::Failed("failed to connect to local tailscaled; it doesn't appear to be running (sudo systemctl start tailscaled ?)".into());
        assert_eq!(setup_step(Err(&old)), 2);
        let new = CliError::Failed("Failed to connect to local Tailscale daemon for /localapi/v0/status; not running?".into());
        assert_eq!(setup_step(Err(&new)), 2);
        assert_eq!(setup_step(Err(&CliError::Failed("something else".into()))), 0);
        assert_eq!(setup_step(Err(&CliError::Timeout(std::time::Duration::from_secs(1)))), 0);
        let login = parse_status(r#"{"BackendState":"NeedsLogin"}"#).unwrap();
        assert_eq!(setup_step(Ok(&login)), 3);
        let stopped = parse_status(r#"{"BackendState":"Stopped"}"#).unwrap();
        assert_eq!(setup_step(Ok(&stopped)), 0);
    }

    fn running(exit: bool) -> Status {
        let exit_status = if exit { r#""ExitNodeStatus":{"ID":"dal"},"# } else { "" };
        let selected = if exit { "true" } else { "false" };
        parse_status(&format!(
            r#"{{"BackendState":"Running",{exit_status}"Self":{{"HostName":"myhost"}},"Peer":{{"a":{{"ID":"dal","HostName":"us-dal-wg-001","DNSName":"us-dal-wg-001.mullvad.ts.net.","Online":true,"ExitNode":{selected},"ExitNodeOption":true,"Location":{{"Country":"USA","City":"Dallas, TX"}}}}}}}}"#
        ))
        .unwrap()
    }

    fn entry(port: u32) -> ServeEntry {
        ServeEntry { port, name: service_name(port), address: String::new(), target: String::new(), public: false }
    }

    #[test]
    fn headline_through_mullvad() {
        let h = headline(Some(&running(true)), None, &[], None);
        assert_eq!(h.tone, Tone::Good);
        assert_eq!(h.title, "Dallas, TX");
        assert_eq!(h.detail, "USA · us-dal-wg-001");
    }

    #[test]
    fn headline_warns_about_home_ip_without_exit_node() {
        let h = headline(Some(&running(false)), None, &[], None);
        assert_eq!(h.tone, Tone::Warn);
        assert!(h.detail.contains("home IP"));
    }

    #[test]
    fn selected_node_that_vanished_is_trouble_not_home_ip() {
        let prefs = Prefs { exit_node_id: Some("gone".into()), ..Prefs::default() };
        let h = headline(Some(&running(false)), Some(&prefs), &[], None);
        assert_eq!(h.tone, Tone::Trouble);
        assert_eq!(h.pill, "Exit node missing");
    }

    #[test]
    fn stopped_headline_names_where_it_resumes() {
        let last = running(true);
        let stopped = parse_status(r#"{"BackendState":"Stopped"}"#).unwrap();
        let prefs = Prefs { exit_node_id: Some("dal".into()), ..Prefs::default() };
        let h = headline(Some(&stopped), Some(&prefs), &last.peers, None);
        assert_eq!(h.tone, Tone::Off);
        assert!(h.detail.contains("Dallas, TX"));
    }

    #[test]
    fn missing_status_shows_the_error() {
        let h = headline(None, None, &[], Some("Couldn't find tailscale."));
        assert_eq!(h.tone, Tone::Trouble);
        assert_eq!(h.detail, "Couldn't find tailscale.");
    }

    #[test]
    fn down_names_everything_it_cuts() {
        let status = running(true);
        let serve = [entry(2222), entry(8443)];
        let text = down_impact(Some(&status), Some(&serve)).join("\n");
        for word in ["Mullvad", "Dallas", "home IP", "Port 2222: stops", "Port 8443: stops", "Remote access", "myhost"] {
            assert!(text.contains(word), "missing {word} in:\n{text}");
        }
    }

    #[test]
    fn down_groups_ports_by_label_and_lists_each_port_once() {
        let mut mail = [entry(465), entry(993), entry(993), entry(8443)];
        mail[0].name = "Mail".into();
        mail[1].name = "Mail".into();
        mail[2].name = "Mail".into();
        mail[3].name = "Dashboard".into();
        let lines = down_impact(None, Some(&mail));
        assert_eq!(lines[0], "Mail: ports 465 and 993 stop answering on the tailnet.");
        assert_eq!(lines[1], "Dashboard: port 8443 stops answering on the tailnet.");
    }

    #[test]
    fn down_says_when_a_service_is_public() {
        let mut funnel = entry(443);
        funnel.public = true;
        let text = down_impact(None, Some(&[funnel])).join("\n");
        assert!(text.contains("Port 443: stops answering on the tailnet and the public internet."), "{text}");
    }

    #[test]
    fn down_admits_it_when_serve_is_unreadable() {
        let text = down_impact(None, None).join("\n");
        assert!(text.contains("couldn't read serve status") && text.contains("Remote access"));
        assert!(!text.contains("Port "), "names no ports it doesn't know about");
    }

    #[test]
    fn down_omits_what_is_not_running_and_names_what_is_there() {
        let status = running(false);
        let text = down_impact(Some(&status), Some(&[entry(8080)])).join("\n");
        assert!(!text.contains("Mullvad"));
        assert!(text.contains("Port 8080") && text.contains("Remote access"));
    }

    #[test]
    fn down_with_nothing_served_only_mentions_remote_access() {
        let lines = down_impact(Some(&running(false)), Some(&[]));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("Remote access"));
    }
}
