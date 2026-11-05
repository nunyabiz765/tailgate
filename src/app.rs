//! Connects the window to the CLI.
//!
//! All state lives on the UI thread. Workers run the CLI and post results back
//! with `slint::invoke_from_event_loop`, so a slow or hung `tailscale` can only
//! ever delay an update, never freeze the window.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use tailgate::cli::{self, Action, CliError, Client, Query};
use tailgate::ipcheck;
use tailgate::nodes;
use tailgate::status::{self, Backend, Peer, Prefs, ServeEntry, Status};
use tailgate::store::{self, Saved};
use tailgate::summary;

use crate::{App as AppGlobal, AppWindow, DeviceRow, NodeRow, ServeRow, Theme};

const POLL_EVERY: Duration = Duration::from_secs(4);
const SERVE_EVERY: Duration = Duration::from_secs(30);
const NOTICE_FADE: Duration = Duration::from_secs(7);
const ADMIN_URL: &str = "https://login.tailscale.com/admin/machines";
const SOURCE_URL: &str = "https://github.com/nunyabiz765/tailgate";

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<Controller>>> = const { RefCell::new(None) };
}

/// Runs `f` on the UI thread with the controller. Safe to call from any thread.
fn post(f: impl FnOnce(&Rc<Controller>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        let controller = CONTROLLER.with(|c| c.borrow().clone());
        if let Some(controller) = controller {
            f(&controller);
        }
    });
}

struct PollResult {
    generation: u64,
    status: Result<Status, CliError>,
    prefs: Result<Prefs, CliError>,
    serve: Option<ServeResult>,
}

struct ServeResult {
    rows: Result<Vec<ServeEntry>, CliError>,
    text: Result<String, CliError>,
}

#[derive(Clone, Copy)]
enum Pending {
    MullvadOff,
    Down,
}

#[derive(Default)]
struct State {
    saved: Saved,
    status: Option<Status>,
    prefs: Option<Prefs>,
    serve: Option<Vec<ServeEntry>>,
    serve_text: String,
    /// Last non-empty exit node list. A stopped daemon reports none.
    known_exits: Vec<Peer>,
    last_error: Option<String>,
    query: String,
    busy: Option<String>,
    confirm: Option<Pending>,
    permission: Option<Action>,
}

struct Controller {
    ui: slint::Weak<AppWindow>,
    client: Client,
    state_path: PathBuf,
    state: RefCell<State>,
    nodes: Rc<VecModel<NodeRow>>,
    devices: Rc<VecModel<DeviceRow>>,
    serve_rows: Rc<VecModel<ServeRow>>,
    notice_timer: slint::Timer,
    /// Wakes the poller early. The value asks it to refresh serve status too.
    poll: Sender<bool>,
    /// Bumped when a change lands, so polls that started before it get dropped
    /// instead of briefly showing the old state.
    generation: Arc<AtomicU64>,
    user: String,
}

fn set_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    // Rebuilding on every poll would reset scroll position and hover.
    let same = model.row_count() == rows.len()
        && rows.iter().enumerate().all(|(i, row)| model.row_data(i).as_ref() == Some(row));
    if !same {
        model.set_vec(rows);
    }
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn os_label(os: &str) -> &str {
    if os.is_empty() {
        "Unknown OS"
    } else {
        os
    }
}

impl Controller {
    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    fn save(&self) {
        let state = self.state.borrow();
        if let Err(e) = store::save(&self.state_path, &state.saved) {
            eprintln!("tailgate: couldn't save {}: {e}", self.state_path.display());
        }
    }

    fn notify(&self, kind: i32, text: impl Into<String>) {
        if let Some(ui) = self.ui() {
            let app = ui.global::<AppGlobal>();
            app.set_notice_kind(kind);
            app.set_notice(text.into().into());
        }
        // Good news fades on its own; errors stay until read.
        self.notice_timer.stop();
        if kind == 1 {
            self.notice_timer.start(slint::TimerMode::SingleShot, NOTICE_FADE, || {
                if let Some(c) = CONTROLLER.with(|c| c.borrow().clone()) {
                    c.notify(0, "");
                }
            });
        }
    }

    /// Keyboard focus can Tab out of a dialog, so check before acting behind one.
    fn modal_open(&self) -> bool {
        let state = self.state.borrow();
        state.confirm.is_some() || state.permission.is_some()
    }

    fn wake_poll(&self, with_serve: bool) {
        let _ = self.poll.send(with_serve);
    }

    // ---- results coming back from workers ----

    fn on_poll(self: &Rc<Self>, result: PollResult) {
        if result.generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            match result.status {
                Ok(status) => {
                    state.last_error = None;
                    let exits: Vec<Peer> = status.exit_nodes().cloned().collect();
                    if !exits.is_empty() {
                        state.known_exits = exits;
                    }
                    state.status = Some(status);
                }
                Err(e) => {
                    state.last_error = Some(e.to_string());
                    // A CLI we can't run or a daemon we can't read means the old
                    // picture may be wrong, so stop presenting it as current.
                    state.status = None;
                }
            }
            // Keep the last good prefs across a one-off failure.
            if let Ok(prefs) = result.prefs {
                state.prefs = Some(prefs);
            }
            if let Some(serve) = result.serve {
                state.serve_text = match serve.text {
                    Ok(text) => text.trim().to_string(),
                    Err(e) => format!("Couldn't read serve status: {e}"),
                };
                state.serve = serve.rows.ok();
            }
            // Keeps "On" able to restore a node that was set outside the app.
            let current = state.status.as_ref().and_then(Status::current_exit_peer).cloned();
            if let Some(peer) = current {
                if state.saved.remember(&peer.id, &peer.place()) {
                    drop(guard);
                    self.save();
                }
            }
        }
        self.render();
    }

    fn on_mutation(self: &Rc<Self>, action: Action, elevated: bool, result: Result<String, CliError>) {
        self.state.borrow_mut().busy = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
        match result {
            Ok(_) => {
                self.notify(1, success_text(&action));
                if matches!(action, Action::SetExitNode { .. } | Action::ClearExitNode | Action::Up | Action::Down) {
                    self.clear_ip_result();
                }
                if let Action::SetOperator(_) = action {
                    // Now the original change should go through without a prompt.
                    let pending = self.state.borrow_mut().permission.take();
                    self.render();
                    if let Some(pending) = pending {
                        self.start(pending, false);
                    }
                    self.wake_poll(false);
                    return;
                }
                self.state.borrow_mut().permission = None;
            }
            Err(CliError::AccessDenied(_)) if !elevated => {
                if let Some(ui) = self.ui() {
                    ui.global::<AppGlobal>().set_permission_lead(
                        format!("Only root can change Tailscale here, so Tailgate couldn't {}.", action.describe())
                            .into(),
                    );
                }
                self.state.borrow_mut().permission = Some(action);
            }
            Err(CliError::AuthDeclined) => {
                self.state.borrow_mut().permission = None;
                self.notify(2, CliError::AuthDeclined.to_string());
            }
            Err(CliError::ReadOnly(cmd)) => {
                self.notify(1, format!("Read-only mode: did not run `{cmd}`."));
            }
            Err(CliError::Timeout(_)) if action == Action::Up => {
                self.state.borrow_mut().permission = None;
                self.notify(2, "Tailscale didn't finish coming up. If it wants a login, a Log in button appears here.");
            }
            Err(CliError::Timeout(_)) => {
                self.state.borrow_mut().permission = None;
                self.notify(2, format!("Tried to {} and got no answer, so it may or may not have applied.", action.describe()));
            }
            Err(e) => {
                self.state.borrow_mut().permission = None;
                self.notify(2, format!("Couldn't {}. {e}", action.describe()));
            }
        }
        self.render();
        self.wake_poll(false);
    }

    fn clear_ip_result(&self) {
        // The answer described the old route, so don't leave it on screen.
        if let Some(ui) = self.ui() {
            ui.global::<AppGlobal>().set_ip_state(0);
        }
    }

    // ---- starting a change ----

    fn start(self: &Rc<Self>, action: Action, elevate: bool) {
        {
            let mut state = self.state.borrow_mut();
            if state.busy.is_some() {
                return;
            }
            state.busy = Some(format!("Working: {}…", action.describe()));
        }
        self.notify(0, "");
        self.render();
        let client = self.client.clone();
        thread::spawn(move || {
            let result = client.mutate(&action, elevate);
            post(move |c| c.on_mutation(action, elevate, result));
        });
    }

    fn find_exit(&self, id: &str) -> Option<Peer> {
        self.state.borrow().known_exits.iter().find(|p| p.id == id).cloned()
    }

    fn switch_to(self: &Rc<Self>, id: &str) {
        let Some(peer) = self.find_exit(id) else {
            self.notify(2, "That exit node is no longer available. Pick another one.");
            return;
        };
        if !peer.online {
            self.notify(2, format!("{} is offline. Pick another one.", peer.place()));
            return;
        }
        let Some(ip) = peer.ipv4() else {
            self.notify(2, format!("{} has no address to connect to.", peer.place()));
            return;
        };
        self.start(Action::SetExitNode { ip: ip.to_string(), label: peer.place() }, false);
    }

    fn toggle_exit(self: &Rc<Self>) {
        if self.exit_active() {
            self.show_confirm(Pending::MullvadOff);
            return;
        }
        let last = self.state.borrow().saved.last_used.clone();
        match last {
            Some(last) if self.find_exit(&last.id).is_some() => self.switch_to(&last.id),
            Some(last) => {
                self.notify(2, format!("{} is no longer available. Pick another exit node.", last.label));
                self.show_nodes_tab();
            }
            None => {
                self.notify(1, "No last-used node yet. Pick one from the list.");
                self.show_nodes_tab();
            }
        }
    }

    fn show_nodes_tab(&self) {
        if let Some(ui) = self.ui() {
            let app = ui.global::<AppGlobal>();
            app.set_tab(if app.get_narrow() { 1 } else { 0 });
        }
    }

    fn show_confirm(&self, pending: Pending) {
        let Some(ui) = self.ui() else { return };
        let app = ui.global::<AppGlobal>();
        let (title, lead, lines, button) = {
            let state = self.state.borrow();
            match pending {
                Pending::MullvadOff => (
                    "Turn off the exit node?",
                    "Sites and services will see your home IP right away. Tailscale itself stays connected."
                        .to_string(),
                    vec![],
                    "Show my home IP",
                ),
                Pending::Down => (
                    "Turn Tailscale off?",
                    "This cuts everything that depends on it:".to_string(),
                    summary::down_impact(state.status.as_ref(), state.serve.as_deref()),
                    "Turn Tailscale off",
                ),
            }
        };
        let lines: Vec<SharedString> = lines.into_iter().map(SharedString::from).collect();
        app.set_confirm_title(title.into());
        app.set_confirm_lead(lead.into());
        app.set_confirm_lines(ModelRc::new(VecModel::from(lines)));
        app.set_confirm_button(button.into());
        app.set_confirm_danger(true);
        self.state.borrow_mut().confirm = Some(pending);
        app.set_confirm_open(true);
    }

    fn exit_active(&self) -> bool {
        let state = self.state.borrow();
        let from_status = state.status.as_ref().is_some_and(|s| s.current_exit.is_some());
        let from_prefs = state.prefs.as_ref().is_some_and(Prefs::has_exit_node);
        from_status || from_prefs
    }

    fn check_ip(self: &Rc<Self>) {
        if let Some(ui) = self.ui() {
            ui.global::<AppGlobal>().set_ip_state(1);
        }
        thread::spawn(move || {
            let result = ipcheck::check();
            post(move |c| {
                let Some(ui) = c.ui() else { return };
                let app = ui.global::<AppGlobal>();
                match result {
                    Ok(info) => {
                        app.set_ip_address(info.ip.clone().into());
                        app.set_ip_place(info.place().into());
                        app.set_ip_mullvad(info.mullvad_line().into());
                        app.set_ip_is_mullvad(info.mullvad_exit_ip);
                        app.set_ip_state(2);
                    }
                    Err(message) => {
                        app.set_ip_error(message.into());
                        app.set_ip_state(3);
                    }
                }
            });
        });
    }

    fn open(&self, url: &str) {
        if let Err(e) = cli::open_url(url) {
            self.notify(2, e.to_string());
        }
    }

    // ---- drawing ----

    fn render(&self) {
        let Some(ui) = self.ui() else { return };
        let app = ui.global::<AppGlobal>();
        let state = self.state.borrow();
        let status = state.status.as_ref();
        let prefs = state.prefs.as_ref();

        let head = summary::headline(status, prefs, &state.known_exits, state.last_error.as_deref());
        app.set_tone(head.tone as i32);
        app.set_pill(head.pill.into());
        app.set_title(head.title.into());
        app.set_detail(head.detail.into());

        let dash = || "—".to_string();
        app.set_machine(status.map(|s| s.self_peer.display_name()).filter(|n| !n.is_empty()).unwrap_or_else(dash).into());
        app.set_tailnet_ip(
            status
                .and_then(|s| s.ips.iter().find(|ip| !ip.contains(':')).or(s.ips.first()).cloned())
                .unwrap_or_else(dash)
                .into(),
        );
        app.set_tailnet(status.map(|s| s.tailnet.clone()).filter(|n| !n.is_empty()).unwrap_or_else(dash).into());
        app.set_health(status.map(|s| s.health.join("\n")).unwrap_or_default().into());

        // Controls. The node ID in prefs survives "down", the one in status doesn't.
        let running = matches!(status.map(|s| &s.backend), Some(Backend::Running));
        let current_peer = status.and_then(Status::current_exit_peer);
        let exit_active = status.is_some_and(|s| s.current_exit.is_some())
            || prefs.is_some_and(Prefs::has_exit_node);
        let mullvadish = match current_peer {
            Some(peer) => peer.mullvad,
            None => state.known_exits.iter().any(|p| p.mullvad) || state.known_exits.is_empty(),
        };
        app.set_exit_active(exit_active);
        app.set_exit_enabled(running);
        app.set_exit_label(if mullvadish { "Route through Mullvad" } else { "Route through exit node" }.into());
        let last_label = state.saved.last_used.as_ref().map(|l| l.label.clone());
        app.set_exit_hint(
            match (status.is_some(), running, exit_active, last_label) {
                (false, ..) => String::new(),
                (true, false, ..) => "Available once Tailscale is up.".to_string(),
                (_, _, true, _) => "Off shows sites your home IP.".to_string(),
                (_, _, false, Some(label)) => format!("On goes back to {label}."),
                (_, _, false, None) => "Pick a node to get started.".to_string(),
            }
            .into(),
        );
        app.set_lan_access(prefs.is_some_and(|p| p.allow_lan));
        app.set_lan_enabled(prefs.is_some() && status.is_some());
        let up = summary::is_up(status);
        let needs_login = matches!(status.map(|s| &s.backend), Some(Backend::NeedsLogin));
        app.set_up(up);
        app.set_up_note(
            match status.map(|s| &s.backend) {
                None => "Unknown",
                Some(Backend::Running) => "Connected",
                Some(Backend::Starting) => "Connecting",
                Some(Backend::NeedsLogin) => "Signed out",
                Some(_) => "Not connected",
            }
            .into(),
        );
        app.set_up_label(if up { "Turn off" } else if needs_login { "Log in" } else { "Bring up" }.into());
        app.set_up_enabled(status.is_some());
        app.set_control_note(
            if prefs.and_then(|p| p.operator.as_deref()) == Some(self.user.as_str()) {
                format!("{} is the operator.\nUndo: sudo tailscale set --operator=", self.user)
            } else {
                String::new()
            }
            .into(),
        );

        app.set_permission_open(state.permission.is_some() && state.busy.is_none());
        app.set_busy(state.busy.is_some());
        app.set_busy_text(state.busy.clone().unwrap_or_default().into());

        // Exit node list: live peers, or the last list seen while Tailscale is off.
        let favorites = state.saved.favorites();
        let current_id = status.and_then(|s| s.current_exit.clone());
        let rows = nodes::build_rows(&state.known_exits, &state.query, state.saved.all_servers, &favorites, current_id.as_deref());
        app.set_node_count(nodes::count_label(&state.known_exits).into());
        app.set_all_servers(state.saved.all_servers);
        app.set_nodes_enabled(running);
        let empty = if !rows.is_empty() {
            String::new()
        } else if state.known_exits.is_empty() {
            if running {
                "This tailnet offers no exit nodes.".to_string()
            } else if status.is_none() {
                "No list until Tailscale answers.".to_string()
            } else {
                "The exit node list appears once Tailscale is up.".to_string()
            }
        } else {
            format!("Nothing matches \"{}\".", state.query.trim())
        };
        app.set_node_empty(empty.into());
        set_model(
            &self.nodes,
            rows.into_iter()
                .map(|r| NodeRow {
                    header: r.header,
                    title: r.title.into(),
                    subtitle: r.subtitle.into(),
                    code: r.code.into(),
                    id: r.id.into(),
                    current: r.current,
                    favorite: r.favorite,
                    selectable: r.selectable,
                })
                .collect(),
        );

        // Devices: the machine itself, then everything that isn't a Mullvad server.
        let mut devices: Vec<DeviceRow> = Vec::new();
        if let Some(s) = status {
            devices.push(DeviceRow {
                name: s.self_peer.display_name().into(),
                detail: os_label(&s.self_peer.os).to_string().into(),
                ip: s.ips.iter().find(|ip| !ip.contains(':')).cloned().unwrap_or_default().into(),
                online: true,
                this_machine: true,
                offers_exit: false,
            });
            let now = now_secs();
            let mut peers: Vec<&Peer> = s.peers.iter().filter(|p| !p.mullvad).collect();
            peers.sort_by_key(|p| (!p.online, p.display_name().to_lowercase()));
            devices.extend(peers.into_iter().map(|p| {
                let seen = if p.online {
                    "online".to_string()
                } else {
                    match p.last_seen.as_deref().and_then(|ts| status::ago(ts, now)) {
                        Some(ago) => format!("last seen {ago}"),
                        None => "offline".to_string(),
                    }
                };
                DeviceRow {
                    name: p.display_name().into(),
                    detail: format!("{} · {seen}", os_label(&p.os)).into(),
                    ip: p.ipv4().unwrap_or("").to_string().into(),
                    online: p.online,
                    this_machine: false,
                    offers_exit: p.exit_option,
                }
            }));
        }
        set_model(&self.devices, devices);

        let serve_rows = state.serve.as_deref().unwrap_or(&[]);
        set_model(
            &self.serve_rows,
            serve_rows
                .iter()
                .map(|e| ServeRow {
                    name: e.name.clone().into(),
                    address: e.address.clone().into(),
                    target: e.target.clone().into(),
                    scope: if e.public { "PUBLIC (Funnel)" } else { "tailnet only" }.into(),
                    public: e.public,
                })
                .collect(),
        );
        app.set_serve_text(state.serve_text.clone().into());
    }
}

fn success_text(action: &Action) -> String {
    match action {
        Action::SetExitNode { label, .. } => format!("Now going out through {label}."),
        Action::ClearExitNode => "Exit node off. Sites see your home IP.".into(),
        Action::AllowLan(true) => "LAN access is on.".into(),
        Action::AllowLan(false) => "LAN access is off.".into(),
        Action::Up => "Tailscale is up.".into(),
        Action::Down => "Tailscale is off.".into(),
        Action::SetOperator(user) => format!("{user} can now change Tailscale without a password."),
    }
}

fn spawn_poller(client: Client, generation: Arc<AtomicU64>, labels: BTreeMap<u32, String>) -> Sender<bool> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut last_serve: Option<Instant> = None;
        let mut force_serve = true;
        loop {
            let generation_now = generation.load(Ordering::SeqCst);
            let serve_due = force_serve || last_serve.is_none_or(|t| t.elapsed() >= SERVE_EVERY);
            let status = client
                .read(Query::Status)
                .and_then(|text| status::parse_status(&text).map_err(CliError::Malformed));
            // If status hung, the rest would too. Say so now rather than after
            // waiting out every timeout in turn.
            let hung = matches!(status, Err(CliError::Timeout(_)));
            let prefs = if hung {
                Err(CliError::Timeout(client.read_timeout))
            } else {
                client.read(Query::Prefs).and_then(|text| status::parse_prefs(&text).map_err(CliError::Malformed))
            };
            let serve = (serve_due && !hung).then(|| {
                last_serve = Some(Instant::now());
                force_serve = false;
                ServeResult {
                    rows: client.read(Query::ServeJson).and_then(|text| {
                        let mut rows = status::parse_serve(&text).map_err(CliError::Malformed)?;
                        status::apply_labels(&mut rows, &labels);
                        Ok(rows)
                    }),
                    text: client.read(Query::ServeText),
                }
            });
            post(move |c| c.on_poll(PollResult { generation: generation_now, status, prefs, serve }));

            match rx.recv_timeout(POLL_EVERY) {
                Ok(with_serve) => force_serve |= with_serve,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    tx
}


pub fn run(read_only: bool) -> Result<(), slint::PlatformError> {
    let client = Client::from_env(read_only);
    let state_path = store::default_path();
    let saved = store::load(&state_path);

    let ui = AppWindow::new()?;
    // Ties the window to tailgate.desktop. Needs the backend `new` just started.
    slint::set_xdg_app_id("tailgate")?;
    ui.global::<Theme>().set_dark(saved.dark);

    let generation = Arc::new(AtomicU64::new(0));
    let labels = store::load_labels(&state_path.with_file_name("labels.json"));
    let poll = spawn_poller(client.clone(), generation.clone(), labels);

    let banner = match (read_only, client.custom_bin) {
        (true, true) => format!("Read-only mode, using a test binary: {}", client.bin.display()),
        (true, false) => "Read-only mode: nothing here can change Tailscale.".to_string(),
        (false, true) => format!("Using a test binary: {}", client.bin.display()),
        (false, false) => String::new(),
    };

    let controller = Rc::new(Controller {
        ui: ui.as_weak(),
        user: cli::current_user().unwrap_or_default(),
        client,
        state_path,
        state: RefCell::new(State { saved, ..State::default() }),
        nodes: Rc::new(VecModel::default()),
        devices: Rc::new(VecModel::default()),
        serve_rows: Rc::new(VecModel::default()),
        notice_timer: slint::Timer::default(),
        poll,
        generation,
    });
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));

    let app = ui.global::<AppGlobal>();
    app.set_banner(banner.into());
    app.set_version(env!("CARGO_PKG_VERSION").into());
    let user = if controller.user.is_empty() { "your user" } else { controller.user.as_str() };
    app.set_operator_detail(
        format!(
            "Runs pkexec tailscale set --operator={user} one time, with your password. After that your \
             account can change Tailscale settings without a password, from Tailgate or the tailscale \
             command. Undo any time: sudo tailscale set --operator="
        )
        .into(),
    );
    app.set_once_detail("Asks for your password and runs only this change as root. Nothing is remembered.".into());
    app.set_nodes(ModelRc::from(controller.nodes.clone()));
    app.set_devices(ModelRc::from(controller.devices.clone()));
    app.set_serve_rows(ModelRc::from(controller.serve_rows.clone()));

    app.on_switch_node(|id| {
        with(move |c| {
            if !c.modal_open() {
                c.switch_to(&id);
            }
        })
    });
    app.on_toggle_favorite(|id| {
        with(move |c| {
            let label = c.find_exit(&id).map(|p| p.place()).unwrap_or_else(|| id.to_string());
            c.state.borrow_mut().saved.toggle_favorite(&id, &label);
            c.save();
            c.render();
        })
    });
    app.on_query_edited(|text| {
        with(move |c| {
            c.state.borrow_mut().query = text.to_string();
            c.render();
        })
    });
    app.on_set_all_servers(|on| {
        with(move |c| {
            c.state.borrow_mut().saved.all_servers = on;
            c.save();
            c.render();
        })
    });
    app.on_toggle_exit(|| {
        with(|c| {
            if !c.modal_open() {
                c.toggle_exit();
            }
        })
    });
    app.on_toggle_lan(|| {
        with(|c| {
            if c.modal_open() {
                return;
            }
            let on = c.state.borrow().prefs.as_ref().is_some_and(|p| p.allow_lan);
            c.start(Action::AllowLan(!on), false);
        })
    });
    app.on_toggle_up(|| {
        with(|c| {
            if c.modal_open() {
                return;
            }
            let (up, login) = {
                let state = c.state.borrow();
                let status = state.status.as_ref();
                (summary::is_up(status), status.map(|s| s.auth_url.clone()).filter(|u| !u.is_empty()))
            };
            if up {
                c.show_confirm(Pending::Down);
            } else if let Some(url) = login {
                c.open(&url);
            } else {
                c.start(Action::Up, false);
            }
        })
    });
    app.on_check_ip(|| with(|c| c.check_ip()));
    app.on_open_admin(|| with(|c| c.open(ADMIN_URL)));
    app.on_open_source(|| with(|c| c.open(SOURCE_URL)));
    app.on_dismiss_notice(|| with(|c| c.notify(0, "")));
    app.on_confirm_cancel(|| {
        with(|c| {
            c.state.borrow_mut().confirm = None;
            if let Some(ui) = c.ui() {
                ui.global::<AppGlobal>().set_confirm_open(false);
            }
        })
    });
    app.on_confirm_accept(|| {
        with(|c| {
            let pending = c.state.borrow_mut().confirm.take();
            if let Some(ui) = c.ui() {
                ui.global::<AppGlobal>().set_confirm_open(false);
            }
            match pending {
                Some(Pending::MullvadOff) => c.start(Action::ClearExitNode, false),
                Some(Pending::Down) => c.start(Action::Down, false),
                None => {}
            }
        })
    });
    app.on_permission_operator(|| {
        with(|c| {
            // `permission` stays set so the original change can retry afterwards.
            if c.user.is_empty() {
                c.notify(2, "Couldn't work out your username.");
                return;
            }
            let action = Action::SetOperator(c.user.clone());
            c.start(action, true);
        })
    });
    app.on_permission_once(|| {
        with(|c| {
            let pending = c.state.borrow_mut().permission.take();
            if let Some(action) = pending {
                c.start(action, true);
            }
        })
    });
    app.on_permission_cancel(|| {
        with(|c| {
            c.state.borrow_mut().permission = None;
            c.notify(1, "Cancelled. Nothing changed.");
            c.render();
        })
    });
    app.on_toggle_theme(|| {
        with(|c| {
            let dark = {
                let mut state = c.state.borrow_mut();
                state.saved.dark = !state.saved.dark;
                state.saved.dark
            };
            c.save();
            if let Some(ui) = c.ui() {
                ui.global::<Theme>().set_dark(dark);
            }
        })
    });
    app.on_tab_changed(|_| with(|c| c.wake_poll(true)));

    controller.render();
    let result = ui.run();
    CONTROLLER.with(|c| *c.borrow_mut() = None);
    result
}

/// Callbacks run on the UI thread, where the controller is already reachable.
fn with(f: impl FnOnce(&Rc<Controller>)) {
    let controller = CONTROLLER.with(|c| c.borrow().clone());
    if let Some(controller) = controller {
        f(&controller);
    }
}
