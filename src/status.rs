//! Parses what the CLI prints: `status --json`, `debug prefs`, `serve status --json`.
//!
//! Missing and null fields are tolerated, because the JSON is documented as
//! subject to change. Prefs deliberately reads only the fields we use, so the
//! keys in `Persist` never enter this process's data.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Running,
    Stopped,
    NeedsLogin,
    NeedsMachineAuth,
    Starting,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub country: String,
    pub code: String,
    pub city: String,
    pub priority: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub id: String,
    pub host: String,
    pub dns: String,
    pub os: String,
    pub ips: Vec<String>,
    pub online: bool,
    pub exit_node: bool,
    pub exit_option: bool,
    pub mullvad: bool,
    pub last_seen: Option<String>,
    pub location: Option<Location>,
}

impl Peer {
    pub fn ipv4(&self) -> Option<&str> {
        self.ips.iter().find(|ip| !ip.contains(':')).or(self.ips.first()).map(String::as_str)
    }

    /// DNS name without the trailing dot.
    pub fn fqdn(&self) -> &str {
        self.dns.trim_end_matches('.')
    }

    /// `us-dal-wg-001` rather than the full name, when it's a Mullvad node.
    pub fn short_name(&self) -> String {
        if self.mullvad {
            self.fqdn().trim_end_matches(".mullvad.ts.net").to_string()
        } else {
            self.display_name()
        }
    }

    pub fn display_name(&self) -> String {
        // iOS reports its hostname as "localhost"; the DNS label is the useful one.
        if self.host.is_empty() || self.host == "localhost" {
            let label = self.fqdn().split('.').next().unwrap_or("");
            if !label.is_empty() {
                return label.to_string();
            }
        }
        self.host.clone()
    }

    /// "Dallas, TX" where the node has a location, its name otherwise.
    pub fn place(&self) -> String {
        match &self.location {
            Some(l) if !l.city.is_empty() => l.city.clone(),
            _ => self.display_name(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub backend: Backend,
    pub self_peer: Peer,
    pub ips: Vec<String>,
    pub tailnet: String,
    pub health: Vec<String>,
    pub auth_url: String,
    /// The exit node's stable ID, when one is in use.
    pub current_exit: Option<String>,
    pub peers: Vec<Peer>,
}

impl Status {
    pub fn exit_nodes(&self) -> impl Iterator<Item = &Peer> {
        self.peers.iter().filter(|p| p.exit_option)
    }

    pub fn current_exit_peer(&self) -> Option<&Peer> {
        match &self.current_exit {
            Some(id) => self.peers.iter().find(|p| &p.id == id),
            None => self.peers.iter().find(|p| p.exit_node),
        }
    }
}

#[derive(Deserialize, Default)]
struct RawLocation {
    #[serde(rename = "Country")]
    country: Option<String>,
    #[serde(rename = "CountryCode")]
    code: Option<String>,
    #[serde(rename = "City")]
    city: Option<String>,
    #[serde(rename = "Priority")]
    priority: Option<i64>,
}

#[derive(Deserialize, Default)]
struct RawPeer {
    #[serde(rename = "ID")]
    id: Option<String>,
    #[serde(rename = "HostName")]
    host: Option<String>,
    #[serde(rename = "DNSName")]
    dns: Option<String>,
    #[serde(rename = "OS")]
    os: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    ips: Option<Vec<String>>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "ExitNode")]
    exit_node: Option<bool>,
    #[serde(rename = "ExitNodeOption")]
    exit_option: Option<bool>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
    #[serde(rename = "LastSeen")]
    last_seen: Option<String>,
    #[serde(rename = "Location")]
    location: Option<RawLocation>,
}

#[derive(Deserialize)]
struct RawTailnet {
    #[serde(rename = "Name")]
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawExitStatus {
    #[serde(rename = "ID")]
    id: Option<String>,
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState")]
    backend: String,
    #[serde(rename = "TailscaleIPs")]
    ips: Option<Vec<String>>,
    #[serde(rename = "Self")]
    self_peer: Option<RawPeer>,
    #[serde(rename = "Peer")]
    peers: Option<BTreeMap<String, RawPeer>>,
    #[serde(rename = "ExitNodeStatus")]
    exit: Option<RawExitStatus>,
    #[serde(rename = "Health")]
    health: Option<Vec<String>>,
    #[serde(rename = "AuthURL")]
    auth_url: Option<String>,
    #[serde(rename = "CurrentTailnet")]
    tailnet: Option<RawTailnet>,
}

fn cook_peer(raw: RawPeer) -> Peer {
    let dns = raw.dns.unwrap_or_default();
    let tags = raw.tags.unwrap_or_default();
    let mullvad = tags.iter().any(|t| t == "tag:mullvad-exit-node")
        || dns.trim_end_matches('.').ends_with(".mullvad.ts.net");
    Peer {
        id: raw.id.unwrap_or_default(),
        host: raw.host.unwrap_or_default(),
        dns,
        os: raw.os.unwrap_or_default(),
        ips: raw.ips.unwrap_or_default(),
        online: raw.online.unwrap_or(false),
        exit_node: raw.exit_node.unwrap_or(false),
        exit_option: raw.exit_option.unwrap_or(false),
        mullvad,
        last_seen: raw.last_seen,
        location: raw.location.map(|l| Location {
            country: l.country.unwrap_or_default(),
            code: l.code.unwrap_or_default(),
            city: l.city.unwrap_or_default(),
            priority: l.priority.unwrap_or(0),
        }),
    }
}

pub fn parse_status(json: &str) -> Result<Status, String> {
    let raw: RawStatus = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let backend = match raw.backend.as_str() {
        "Running" => Backend::Running,
        "Stopped" => Backend::Stopped,
        "NeedsLogin" => Backend::NeedsLogin,
        "NeedsMachineAuth" => Backend::NeedsMachineAuth,
        "Starting" => Backend::Starting,
        other => Backend::Other(other.to_string()),
    };
    Ok(Status {
        backend,
        self_peer: cook_peer(raw.self_peer.unwrap_or_default()),
        ips: raw.ips.unwrap_or_default(),
        tailnet: raw.tailnet.and_then(|t| t.name).unwrap_or_default(),
        health: raw.health.unwrap_or_default(),
        auth_url: raw.auth_url.unwrap_or_default(),
        current_exit: raw.exit.and_then(|e| e.id).filter(|id| !id.is_empty()),
        peers: raw.peers.unwrap_or_default().into_values().map(cook_peer).collect(),
    })
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Prefs {
    #[serde(rename = "ExitNodeID", default)]
    pub exit_node_id: Option<String>,
    #[serde(rename = "ExitNodeAllowLANAccess", default)]
    pub allow_lan: bool,
    #[serde(rename = "OperatorUser", default)]
    pub operator: Option<String>,
    #[serde(rename = "WantRunning", default)]
    pub want_running: bool,
}

impl Prefs {
    pub fn has_exit_node(&self) -> bool {
        self.exit_node_id.as_deref().is_some_and(|id| !id.is_empty())
    }
}

pub fn parse_prefs(json: &str) -> Result<Prefs, String> {
    serde_json::from_str(json).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeEntry {
    pub port: u32,
    pub name: String,
    pub address: String,
    pub target: String,
    pub public: bool,
}

#[derive(Deserialize, Default)]
struct RawTcp {
    #[serde(rename = "TCPForward")]
    forward: Option<String>,
    #[serde(rename = "TerminateTLS")]
    tls_host: Option<String>,
    #[serde(rename = "HTTPS")]
    https: Option<bool>,
    #[serde(rename = "HTTP")]
    http: Option<bool>,
}

#[derive(Deserialize, Default)]
struct RawHandler {
    #[serde(rename = "Proxy")]
    proxy: Option<String>,
    #[serde(rename = "Path")]
    path: Option<String>,
    #[serde(rename = "Text")]
    text: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawWeb {
    #[serde(rename = "Handlers")]
    handlers: Option<BTreeMap<String, RawHandler>>,
}

#[derive(Deserialize, Default)]
struct RawServe {
    #[serde(rename = "TCP")]
    tcp: Option<BTreeMap<String, RawTcp>>,
    #[serde(rename = "Web")]
    web: Option<BTreeMap<String, RawWeb>>,
    #[serde(rename = "AllowFunnel")]
    funnel: Option<BTreeMap<String, bool>>,
}

/// What a served port is called until the user's labels.json says otherwise.
pub fn service_name(port: u32) -> String {
    format!("Port {port}")
}

/// Swaps in the user's own names for the ports they've labelled.
pub fn apply_labels(entries: &mut [ServeEntry], labels: &BTreeMap<u32, String>) {
    for entry in entries {
        if let Some(label) = labels.get(&entry.port) {
            entry.name.clone_from(label);
        }
    }
}

pub fn parse_serve(json: &str) -> Result<Vec<ServeEntry>, String> {
    let raw: RawServe = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let funnel = raw.funnel.unwrap_or_default();
    let mut out = Vec::new();

    for (hostport, web) in raw.web.unwrap_or_default() {
        let port = hostport.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let public = funnel.get(&hostport).copied().unwrap_or(false);
        for (path, handler) in web.handlers.unwrap_or_default() {
            let target = handler
                .proxy
                .or(handler.path)
                .or(handler.text.map(|_| "static text".to_string()))
                .unwrap_or_default();
            let suffix = if path == "/" { "" } else { path.as_str() };
            out.push(ServeEntry {
                port,
                name: service_name(port),
                address: format!("https://{hostport}{suffix}"),
                target,
                public,
            });
        }
    }

    for (port_text, tcp) in raw.tcp.unwrap_or_default() {
        // HTTP(S) ports are already covered by their Web entry.
        if tcp.https.unwrap_or(false) || tcp.http.unwrap_or(false) {
            continue;
        }
        let port: u32 = port_text.parse().unwrap_or(0);
        let host = tcp.tls_host.as_deref().unwrap_or("this machine");
        let address = format!("{host}:{port}");
        out.push(ServeEntry {
            port,
            name: service_name(port),
            public: funnel.get(&address).copied().unwrap_or(false),
            address,
            target: tcp.forward.unwrap_or_default(),
        });
    }

    out.sort_by_key(|e| e.port);
    Ok(out)
}

/// Seconds since the Unix epoch for `2026-07-08T00:18:20.1Z`. UTC only, which
/// is all Tailscale emits. The Go zero time comes back as None.
pub fn parse_timestamp(ts: &str) -> Option<i64> {
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    if y < 1970 || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let time = time.trim_end_matches('Z');
    let mut t = time.split(':');
    let (h, min): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let sec: i64 = t.next()?.split('.').next()?.parse().ok()?;
    // Days from civil, Howard Hinnant's algorithm.
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + min * 60 + sec)
}

pub fn ago(ts: &str, now: i64) -> Option<String> {
    let secs = (now - parse_timestamp(ts)?).max(0);
    let (n, unit) = match secs {
        s if s < 90 => return Some("just now".into()),
        s if s < 3600 => (s / 60, "minute"),
        s if s < 86400 => (s / 3600, "hour"),
        s if s < 86400 * 30 => (s / 86400, "day"),
        s if s < 86400 * 365 => (s / (86400 * 30), "month"),
        s => (s / (86400 * 365), "year"),
    };
    Some(format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "BackendState": "Running",
      "TailscaleIPs": ["100.1.2.3", "fd7a::1"],
      "Self": {"ID":"self","HostName":"myhost","DNSName":"myhost.example.ts.net.","OS":"linux","TailscaleIPs":["100.1.2.3"],"Online":true},
      "ExitNodeStatus": {"ID":"dal1","Online":true,"TailscaleIPs":["100.64.0.10/32"]},
      "Health": ["some warning"],
      "CurrentTailnet": {"Name":"me@example.com"},
      "Peer": {
        "k1": {"ID":"dal1","HostName":"us-dal-wg-001","DNSName":"us-dal-wg-001.mullvad.ts.net.","TailscaleIPs":["100.64.0.10","fd7a::2"],"Online":true,"ExitNode":true,"ExitNodeOption":true,"Tags":["tag:mullvad-exit-node"],"Location":{"Country":"USA","CountryCode":"US","City":"Dallas, TX","Priority":100}},
        "k2": {"ID":"mac","HostName":"macbook","DNSName":"macbook.example.ts.net.","OS":"macOS","TailscaleIPs":null,"Online":false,"Tags":null,"LastSeen":"2026-07-08T00:18:20.1Z"},
        "k3": {"ID":"ph","HostName":"localhost","DNSName":"myphone.example.ts.net.","OS":"iOS","Online":true}
      }
    }"#;

    #[test]
    fn parses_status() {
        let s = parse_status(STATUS).unwrap();
        assert_eq!(s.backend, Backend::Running);
        assert_eq!(s.current_exit.as_deref(), Some("dal1"));
        assert_eq!(s.tailnet, "me@example.com");
        assert_eq!(s.exit_nodes().count(), 1);
        let exit = s.current_exit_peer().unwrap();
        assert!(exit.mullvad);
        assert_eq!(exit.short_name(), "us-dal-wg-001");
        assert_eq!(exit.place(), "Dallas, TX");
        assert_eq!(exit.ipv4(), Some("100.64.0.10"));
    }

    #[test]
    fn null_fields_and_ios_hostname_are_fine() {
        let s = parse_status(STATUS).unwrap();
        let mac = s.peers.iter().find(|p| p.id == "mac").unwrap();
        assert!(mac.ips.is_empty() && mac.ipv4().is_none() && !mac.exit_option);
        let phone = s.peers.iter().find(|p| p.id == "ph").unwrap();
        assert_eq!(phone.display_name(), "myphone");
    }

    #[test]
    fn stopped_daemon_has_no_peers_or_exit_status() {
        let s = parse_status(r#"{"BackendState":"Stopped","Peer":null,"Self":null,"ExitNodeStatus":null}"#).unwrap();
        assert_eq!(s.backend, Backend::Stopped);
        assert!(s.peers.is_empty() && s.current_exit.is_none());
    }

    #[test]
    fn malformed_status_is_an_error_not_a_panic() {
        for bad in ["", "not json", "{}", "[]", r#"{"BackendState":7}"#, r#"{"BackendState":"Running","Peer":"x"}"#] {
            assert!(parse_status(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn prefs_ignores_everything_else() {
        let p = parse_prefs(r#"{"ExitNodeID":"abc","ExitNodeAllowLANAccess":true,"WantRunning":true,"Persist":{"PrivateNodeKey":"x"}}"#).unwrap();
        assert!(p.has_exit_node() && p.allow_lan && p.operator.is_none());
        let p = parse_prefs(r#"{"ExitNodeID":"","OperatorUser":"pat"}"#).unwrap();
        assert!(!p.has_exit_node());
        assert_eq!(p.operator.as_deref(), Some("pat"));
        assert!(parse_prefs("oops").is_err());
    }

    #[test]
    fn parses_serve_config_with_web_and_tcp() {
        let json = r#"{
          "TCP": {"2222":{"TCPForward":"127.0.0.1:22","TerminateTLS":"h.ts.net"},"8443":{"HTTPS":true},"5432":{"TCPForward":"127.0.0.1:5432","TerminateTLS":"h.ts.net"}},
          "Web": {"h.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}
        }"#;
        let mut rows = parse_serve(json).unwrap();
        assert_eq!(rows.iter().map(|r| r.port).collect::<Vec<_>>(), [2222, 5432, 8443]);
        assert_eq!(rows[2].name, "Port 8443");
        assert_eq!(rows[2].address, "https://h.ts.net:8443");
        assert_eq!(rows[2].target, "http://127.0.0.1:3000");
        assert_eq!(rows[0].target, "127.0.0.1:22");
        assert!(rows.iter().all(|r| !r.public));

        apply_labels(&mut rows, &BTreeMap::from([(8443, "Dashboard".to_string())]));
        assert_eq!(rows[2].name, "Dashboard");
        assert_eq!(rows[0].name, "Port 2222", "unlabelled ports stay as they were");
    }

    #[test]
    fn empty_and_funnel_serve_configs() {
        assert!(parse_serve("{}").unwrap().is_empty());
        let rows = parse_serve(r#"{"Web":{"h:443":{"Handlers":{"/a":{"Proxy":"http://x"}}}},"AllowFunnel":{"h:443":true}}"#).unwrap();
        assert!(rows[0].public);
        assert_eq!(rows[0].address, "https://h:443/a");
        assert!(parse_serve("nope").is_err());
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2026-07-08T00:18:20.1Z"), Some(1783469900));
        assert_eq!(parse_timestamp("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_timestamp("garbage"), None);
        let now = parse_timestamp("2026-07-10T00:18:20Z").unwrap();
        assert_eq!(ago("2026-07-08T00:18:20Z", now).unwrap(), "2 days ago");
        assert_eq!(ago("2026-07-10T00:18:00Z", now).unwrap(), "just now");
        assert_eq!(ago("2026-07-09T23:18:20Z", now).unwrap(), "1 hour ago");
    }
}
