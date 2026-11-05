//! The manual "Check my IP" button. Uses curl rather than bundling a TLS stack,
//! and is only ever called from a click.

use crate::cli::{run, CliError};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

pub const URL: &str = "https://am.i.mullvad.net/json";

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IpInfo {
    pub ip: String,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub mullvad_exit_ip: bool,
    #[serde(default)]
    pub mullvad_exit_ip_hostname: Option<String>,
}

impl IpInfo {
    pub fn place(&self) -> String {
        match (&self.city, &self.country) {
            (Some(city), Some(country)) => format!("{city}, {country}"),
            (Some(place), None) | (None, Some(place)) => place.clone(),
            (None, None) => "Unknown location".into(),
        }
    }

    pub fn mullvad_line(&self) -> String {
        match (self.mullvad_exit_ip, &self.mullvad_exit_ip_hostname) {
            (true, Some(host)) => format!("Mullvad is in use ({host})"),
            (true, None) => "Mullvad is in use".into(),
            (false, _) => "Mullvad is not in use".into(),
        }
    }
}

pub fn parse(body: &str) -> Result<IpInfo, String> {
    let info: IpInfo = serde_json::from_str(body).map_err(|_| "Mullvad's answer wasn't readable.".to_string())?;
    if info.ip.is_empty() {
        return Err("Mullvad's answer had no IP address.".into());
    }
    Ok(info)
}

/// `TAILGATE_IPCHECK_URL` lets tests avoid the network.
pub fn check() -> Result<IpInfo, String> {
    let url = std::env::var("TAILGATE_IPCHECK_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| URL.to_string());
    fetch(&url)
}

pub fn fetch(url: &str) -> Result<IpInfo, String> {
    let args = ["--silent", "--show-error", "--fail", "--max-time", "12", "--user-agent", "Tailgate", url]
        .map(String::from);
    match run(Path::new("curl"), &args, Duration::from_secs(15)) {
        Ok(out) if out.success => parse(&out.stdout),
        Ok(out) => {
            let reason = out.stderr.trim().trim_start_matches("curl: ").to_string();
            Err(format!("Couldn't reach Mullvad's checker. {reason}").trim().to_string())
        }
        Err(CliError::Missing(_)) => Err("curl isn't installed, so Tailgate can't check.".into()),
        Err(e) => Err(format!("Couldn't reach Mullvad's checker. {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_mullvad_answer() {
        let info = parse(r#"{"ip":"198.51.100.7","country":"USA","city":"Dallas, TX","mullvad_exit_ip":true,"mullvad_exit_ip_hostname":"us-dal-wg-001","blacklisted":{"blacklisted":false,"results":[]}}"#).unwrap();
        assert_eq!(info.place(), "Dallas, TX, USA");
        assert_eq!(info.mullvad_line(), "Mullvad is in use (us-dal-wg-001)");
    }

    #[test]
    fn parses_a_home_ip_answer() {
        let info = parse(r#"{"ip":"203.0.113.9","city":null,"country":"USA","mullvad_exit_ip":false,"mullvad_exit_ip_hostname":null}"#).unwrap();
        assert_eq!(info.place(), "USA");
        assert_eq!(info.mullvad_line(), "Mullvad is not in use");
    }

    #[test]
    fn bad_answers_are_errors() {
        for bad in ["", "<html>busy</html>", "{}", r#"{"ip":""}"#, "[1]"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn unreachable_server_is_a_message_not_a_panic() {
        let err = fetch("file:///nonexistent/tailgate/path").unwrap_err();
        assert!(err.starts_with("Couldn't reach"), "{err}");
    }
}
