//! Turns the peer list into the rows the exit node picker shows.

use crate::status::Peer;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    pub header: bool,
    pub title: String,
    pub subtitle: String,
    pub code: String,
    pub id: String,
    pub current: bool,
    pub favorite: bool,
    /// False for offline nodes and for favorites that have vanished.
    pub selectable: bool,
}

/// A favorite remembered by stable ID, with its name in case the node vanishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Favorite {
    pub id: String,
    pub label: String,
}

/// Lowercases and drops accents, so "zurich" finds "Zürich" and "sao" finds "São".
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'ç' | 'ć' | 'č' => out.push('c'),
            'ď' | 'đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ğ' => out.push('g'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ı' => out.push('i'),
            'ł' | 'ľ' => out.push('l'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
            'ř' => out.push('r'),
            'ś' | 'š' | 'ş' | 'ș' => out.push('s'),
            'ß' => out.push_str("ss"),
            'ť' | 'ț' | 'ţ' => out.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'ź' | 'ż' | 'ž' => out.push('z'),
            other => out.push(other),
        }
    }
    out
}

fn haystack(peer: &Peer) -> String {
    let mut text = format!("{} {} {}", peer.fqdn(), peer.host, peer.ips.join(" "));
    if let Some(loc) = &peer.location {
        text.push_str(&format!(" {} {} {}", loc.city, loc.country, loc.code));
        // What people type for the two countries whose names they abbreviate.
        match loc.code.as_str() {
            "US" => text.push_str(" united states america"),
            "GB" => text.push_str(" uk britain england"),
            _ => {}
        }
    }
    fold(&text)
}

pub fn matches(peer: &Peer, query: &str) -> bool {
    let hay = haystack(peer);
    fold(query).split_whitespace().all(|token| hay.contains(token))
}

fn sort_key(peer: &Peer) -> (i64, String) {
    // Higher priority first, then name, the same order `exit-node list` uses.
    let priority = peer.location.as_ref().map_or(0, |l| l.priority);
    (-priority, peer.dns.clone())
}

fn node_row(peer: &Peer, current: Option<&str>, favorites: &[Favorite], servers: usize) -> Row {
    let mut subtitle = if peer.mullvad {
        peer.short_name()
    } else {
        format!("On your tailnet · {}", peer.ipv4().unwrap_or("no address"))
    };
    if servers > 1 {
        subtitle.push_str(&format!(" · {servers} servers"));
    }
    if !peer.online {
        subtitle.push_str(" · offline");
    }
    Row {
        header: false,
        title: peer.place(),
        subtitle,
        code: peer.location.as_ref().map_or_else(String::new, |l| l.code.clone()),
        id: peer.id.clone(),
        current: current == Some(peer.id.as_str()),
        favorite: favorites.iter().any(|f| f.id == peer.id),
        selectable: peer.online,
    }
}

fn header(title: &str) -> Row {
    Row { header: true, title: title.to_string(), ..Row::default() }
}

/// Builds the rows. With `all_servers` off each city shows one node, the highest
/// priority match for the query.
pub fn build_rows(
    peers: &[Peer],
    query: &str,
    all_servers: bool,
    favorites: &[Favorite],
    current: Option<&str>,
) -> Vec<Row> {
    let exits: Vec<&Peer> = peers.iter().filter(|p| p.exit_option).collect();
    let matched: Vec<&Peer> = exits.iter().copied().filter(|p| matches(p, query)).collect();
    let mut rows = Vec::new();

    // The list is long, so the connected node gets pinned where it can be seen,
    // and only there: everything below is somewhere you could switch to.
    if let Some(peer) = matched.iter().find(|p| current == Some(p.id.as_str())) {
        rows.push(header("Connected"));
        rows.push(node_row(peer, current, favorites, 1));
    }
    let matched: Vec<&Peer> = matched.into_iter().filter(|p| current != Some(p.id.as_str())).collect();

    // Favorites first, including ones that have gone away so they can be unstarred.
    let mut fav_rows = Vec::new();
    for fav in favorites {
        match exits.iter().find(|p| p.id == fav.id) {
            Some(peer) if current == Some(peer.id.as_str()) => {}
            Some(peer) if matches(peer, query) => fav_rows.push(node_row(peer, current, favorites, 1)),
            Some(_) => {}
            // An empty peer list means we don't know yet, not that the node is gone.
            None if !exits.is_empty() && fold(&fav.label).contains(fold(query).trim()) => fav_rows.push(Row {
                title: fav.label.clone(),
                subtitle: "No longer available".into(),
                id: fav.id.clone(),
                favorite: true,
                ..Row::default()
            }),
            None => {}
        }
    }
    if !fav_rows.is_empty() {
        rows.push(header("Favorites"));
        rows.append(&mut fav_rows);
    }

    // Exit nodes on the tailnet itself have no Mullvad location.
    let mut own: Vec<&Peer> = matched.iter().copied().filter(|p| !p.mullvad).collect();
    own.sort_by_key(|p| p.display_name());
    if !own.is_empty() {
        rows.push(header("Your tailnet"));
        rows.extend(own.iter().map(|p| node_row(p, current, favorites, 1)));
    }

    // country -> city -> peers
    let mut countries: BTreeMap<String, BTreeMap<String, Vec<&Peer>>> = BTreeMap::new();
    for peer in matched.iter().copied().filter(|p| p.mullvad) {
        let (country, city) = match &peer.location {
            Some(l) => (l.country.clone(), l.city.clone()),
            None => (String::from("Other"), peer.display_name()),
        };
        countries.entry(country).or_default().entry(city).or_default().push(peer);
    }

    let city_totals = |code_city: (&str, &str)| {
        exits
            .iter()
            .filter(|p| p.location.as_ref().is_some_and(|l| l.country == code_city.0 && l.city == code_city.1))
            .count()
    };

    for (country, cities) in countries {
        rows.push(header(&country));
        for (city, mut peers) in cities {
            peers.sort_by_key(|p| sort_key(p));
            if all_servers {
                rows.extend(peers.iter().map(|p| node_row(p, current, favorites, 1)));
            } else {
                rows.push(node_row(peers[0], current, favorites, city_totals((&country, &city))));
            }
        }
    }
    rows
}

/// "91 cities · 528 servers"
pub fn count_label(peers: &[Peer]) -> String {
    let exits: Vec<&Peer> = peers.iter().filter(|p| p.exit_option).collect();
    let mut cities: Vec<String> = exits
        .iter()
        .map(|p| match &p.location {
            Some(l) => format!("{}/{}", l.code, l.city),
            None => p.id.clone(),
        })
        .collect();
    cities.sort();
    cities.dedup();
    let plural = |n: usize, word: &str, words: &str| format!("{n} {}", if n == 1 { word } else { words });
    format!("{} · {}", plural(cities.len(), "place", "places"), plural(exits.len(), "server", "servers"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Location;

    fn node(id: &str, host: &str, country: &str, code: &str, city: &str, prio: i64) -> Peer {
        Peer {
            id: id.into(),
            host: host.into(),
            dns: format!("{host}.mullvad.ts.net."),
            os: String::new(),
            ips: vec![format!("100.64.0.{}", id.len())],
            online: true,
            exit_node: false,
            exit_option: true,
            mullvad: true,
            last_seen: None,
            location: Some(Location { country: country.into(), code: code.into(), city: city.into(), priority: prio }),
        }
    }

    fn sample() -> Vec<Peer> {
        vec![
            node("dal1", "us-dal-wg-001", "USA", "US", "Dallas, TX", 100),
            node("dal2", "us-dal-wg-002", "USA", "US", "Dallas, TX", 90),
            node("lax1", "us-lax-wg-402", "USA", "US", "Los Angeles, CA", 100),
            node("zur1", "ch-zrh-wg-001", "Switzerland", "CH", "Zürich", 100),
            node("lon1", "gb-lon-wg-001", "UK", "GB", "London", 100),
        ]
    }

    fn own_exit() -> Peer {
        Peer {
            id: "home".into(),
            host: "homebox".into(),
            dns: "homebox.example.ts.net.".into(),
            mullvad: false,
            location: None,
            ..node("home", "homebox", "", "", "", 0)
        }
    }

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter().filter(|r| !r.header).map(|r| r.id.clone()).collect()
    }

    #[test]
    fn one_row_per_city_by_default_grouped_by_country() {
        let rows = build_rows(&sample(), "", false, &[], None);
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, ["Switzerland", "Zürich", "UK", "London", "USA", "Dallas, TX", "Los Angeles, CA"]);
        let dallas = rows.iter().find(|r| r.title == "Dallas, TX").unwrap();
        assert_eq!(dallas.id, "dal1", "highest priority wins");
        assert_eq!(dallas.subtitle, "us-dal-wg-001 · 2 servers");
    }

    #[test]
    fn all_servers_lists_every_node() {
        assert_eq!(ids(&build_rows(&sample(), "", true, &[], None)).len(), 5);
    }

    #[test]
    fn current_node_is_pinned_once_and_its_city_offers_the_next_server() {
        let rows = build_rows(&sample(), "", false, &[], Some("dal1"));
        assert_eq!(rows[0].title, "Connected");
        assert_eq!(rows[1].id, "dal1");
        assert!(rows[1].current);
        assert_eq!(rows.iter().filter(|r| r.current).count(), 1);
        assert_eq!(rows.iter().filter(|r| r.id == "dal1").count(), 1);
        let other = rows.iter().find(|r| r.title == "Dallas, TX" && !r.current).unwrap();
        assert_eq!(other.id, "dal2");
    }

    #[test]
    fn current_node_is_not_repeated_under_favorites() {
        let favs = vec![Favorite { id: "dal1".into(), label: "Dallas, TX".into() }];
        let rows = build_rows(&sample(), "", false, &favs, Some("dal1"));
        assert!(rows[1].favorite, "pinned row keeps its star");
        assert!(!rows.iter().any(|r| r.title == "Favorites"));
    }

    #[test]
    fn a_search_that_misses_the_current_node_does_not_pin_it() {
        let rows = build_rows(&sample(), "zurich", false, &[], Some("dal1"));
        assert_eq!(ids(&rows), ["zur1"]);
    }

    #[test]
    fn search_covers_city_country_host_and_accents() {
        let find = |q: &str| ids(&build_rows(&sample(), q, false, &[], None));
        assert_eq!(find("dallas"), ["dal1"]);
        assert_eq!(find("switzerland"), ["zur1"]);
        assert_eq!(find("zurich"), ["zur1"]);
        assert_eq!(find("ZÜRICH"), ["zur1"]);
        assert_eq!(find("united states").len(), 2);
        assert_eq!(find("uk"), ["lon1"]);
        assert_eq!(find("us dal"), ["dal1"], "every word must match");
        assert_eq!(find("us-dal-wg-002"), ["dal2"], "a hostname reaches past the best node");
        assert!(find("atlantis").is_empty());
    }

    #[test]
    fn non_mullvad_exit_nodes_get_their_own_section() {
        let mut peers = sample();
        peers.push(own_exit());
        let rows = build_rows(&peers, "", false, &[], None);
        assert_eq!(rows[0].title, "Your tailnet");
        assert_eq!(rows[1].id, "home");
        assert_eq!(rows[1].title, "homebox");
    }

    #[test]
    fn non_exit_peers_never_appear() {
        let mut peers = sample();
        let mut laptop = own_exit();
        laptop.exit_option = false;
        peers.push(laptop);
        assert!(!ids(&build_rows(&peers, "", true, &[], None)).contains(&"home".to_string()));
    }

    #[test]
    fn favorites_float_to_the_top_and_stay_in_their_city() {
        let favs = vec![Favorite { id: "zur1".into(), label: "Zürich".into() }];
        let rows = build_rows(&sample(), "", false, &favs, None);
        assert_eq!(rows[0].title, "Favorites");
        assert_eq!(rows[1].id, "zur1");
        assert!(rows[1].favorite);
        assert_eq!(rows.iter().filter(|r| r.id == "zur1").count(), 2);
    }

    #[test]
    fn vanished_favorite_is_visible_but_not_selectable() {
        let favs = vec![Favorite { id: "gone".into(), label: "Atlantis".into() }];
        let rows = build_rows(&sample(), "", false, &favs, None);
        assert_eq!(rows[1].title, "Atlantis");
        assert_eq!(rows[1].subtitle, "No longer available");
        assert!(!rows[1].selectable);
        // With no peer list at all we can't say it has gone.
        assert!(build_rows(&[], "", false, &favs, None).is_empty());
    }

    #[test]
    fn offline_node_is_labeled_and_not_selectable() {
        let mut peers = sample();
        peers[3].online = false;
        let row = build_rows(&peers, "zurich", false, &[], None).pop().unwrap();
        assert!(!row.selectable);
        assert!(row.subtitle.ends_with("offline"));
    }

    #[test]
    fn count_label_counts_places_and_servers() {
        assert_eq!(count_label(&sample()), "4 places · 5 servers");
    }
}
