//! Connects the window to the CLI.
//!
//! All state lives on the UI thread. Workers run the CLI and post results back
//! with `slint::invoke_from_event_loop`, so a slow or hung `tailscale` can only
//! ever delay an update, never freeze the window.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use tailgate::autostart;
use tailgate::cli::{self, Action, CliError, Client, Query};
use tailgate::ipcheck;
use tailgate::nodes;
use tailgate::shares::{self, Mounted, MountKind, Outcome, Reconnect, ShareRef, Tools, Want};
use tailgate::status::{self, Backend, Peer, Prefs, ServeEntry, Status};
use tailgate::store::{self, Saved, SavedShare};
use tailgate::summary;

use crate::{App as AppGlobal, AppWindow, DeviceRow, NodeRow, ServeRow, ShareRow, Theme};

const POLL_EVERY: Duration = Duration::from_secs(4);
const SERVE_EVERY: Duration = Duration::from_secs(30);
const NOTICE_FADE: Duration = Duration::from_secs(7);
/// How long "Waiting for Finder" shows before Tailgate stops expecting the mount.
const FINDER_WAIT: Duration = Duration::from_secs(90);
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
    mounted: Vec<Mounted>,
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
    /// First-run guide step on Linux (see summary::setup_step); 0 hides it.
    setup: i32,
    /// "Sign in" was clicked, so open the login page as soon as Tailscale offers one.
    login_requested: bool,
    shares: SharesView,
}

/// What the Shares tab knows beyond the saved shares.
#[derive(Default)]
struct SharesView {
    found: Vec<shares::Found>,
    scanning: bool,
    scanned: bool,
    mounted: Vec<Mounted>,
    /// Connects and disconnects in flight.
    working: BTreeSet<String>,
    /// Handed to Finder, which mounts on its own time.
    requested: BTreeMap<String, Instant>,
    /// The last thing worth saying about a share: (row state, text).
    notes: BTreeMap<String, (i32, String)>,
    reconnect: Reconnect,
    was_running: Option<bool>,
    online: BTreeMap<String, bool>,
    last_poll: Option<(SystemTime, Instant)>,
    launch_at_login: bool,
}

struct Controller {
    ui: slint::Weak<AppWindow>,
    client: Client,
    tools: Tools,
    state_path: PathBuf,
    state: RefCell<State>,
    nodes: Rc<VecModel<NodeRow>>,
    devices: Rc<VecModel<DeviceRow>>,
    serve_rows: Rc<VecModel<ServeRow>>,
    share_rows: Rc<VecModel<ShareRow>>,
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
        let add_share = self.ui().is_some_and(|ui| ui.global::<AppGlobal>().get_add_share_open());
        state.confirm.is_some() || state.permission.is_some() || add_share
    }

    fn wake_poll(&self, with_serve: bool) {
        let _ = self.poll.send(with_serve);
    }

    // ---- results coming back from workers ----

    fn on_poll(self: &Rc<Self>, result: PollResult) {
        if result.generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        let mut login_url = None;
        {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            // The guide is for Linux, where Tailgate can't point at an installer app.
            state.setup = if cfg!(target_os = "linux") { summary::setup_step(result.status.as_ref()) } else { 0 };
            if state.login_requested {
                if let Ok(url) = result.status.as_ref().map(|s| s.auth_url.as_str()) {
                    if !url.is_empty() {
                        state.login_requested = false;
                        login_url = Some(url.to_string());
                    }
                }
            }
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
        if let Some(url) = login_url {
            self.open(&url);
        }
        self.after_poll_shares(result.mounted);
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
            // No pkexec on Windows or macOS, so explain instead of offering a prompt.
            Err(CliError::AccessDenied(msg)) if !cli::ELEVATION => {
                self.state.borrow_mut().permission = None;
                self.notify(2, format!("Couldn't {}. Tailscale refused: {msg} {}", action.describe(), denied_hint(self.client.variant)));
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

    // ---- file shares ----

    fn shares_tab(&self) -> i32 {
        self.ui().map_or(3, |ui| if ui.global::<AppGlobal>().get_narrow() { 4 } else { 3 })
    }

    /// Every name a share's host goes by on the tailnet, so a mount made with
    /// the IP matches a share saved by name and the other way round.
    fn host_aliases(&self, host: &str) -> Vec<String> {
        let state = self.state.borrow();
        let mut out = vec![host.to_string()];
        if let Some(peer) = state.status.as_ref().and_then(|s| peer_for(s, host)) {
            out.extend(peer.ips.iter().cloned());
            out.push(peer.host.clone());
            out.push(peer.fqdn().to_string());
            out.push(peer.display_name());
        }
        out
    }

    fn mounted_for(&self, key: &str) -> Option<Mounted> {
        let (host, share) = self.share_parts(key)?;
        let aliases = self.host_aliases(&host);
        self.state.borrow().shares.mounted.iter().find(|m| m.is(&share, &aliases)).cloned()
    }

    /// Host and share name for a key, from the saved shares, the scan or the mounts.
    fn share_parts(&self, key: &str) -> Option<(String, String)> {
        let state = self.state.borrow();
        if let Some(s) = state.saved.share(key) {
            return Some((s.host.clone(), s.share.clone()));
        }
        for f in &state.shares.found {
            for name in f.shares.iter().flatten() {
                if shares::share_key(&f.host, name) == key {
                    return Some((f.host.clone(), name.clone()));
                }
            }
        }
        state
            .shares
            .mounted
            .iter()
            .find(|m| shares::share_key(&m.host, &m.share) == key)
            .map(|m| (m.host.clone(), m.share.clone()))
    }

    fn share_ref(&self, key: &str) -> Option<ShareRef> {
        let user = self.state.borrow().saved.share(key).map(|s| s.user.clone()).unwrap_or_default();
        let (host, share) = self.share_parts(key)?;
        ShareRef::new(&host, &share, &user).ok()
    }

    fn device_name(&self, host: &str) -> String {
        let state = self.state.borrow();
        if let Some(peer) = state.status.as_ref().and_then(|s| peer_for(s, host)) {
            return peer.display_name();
        }
        state
            .saved
            .shares
            .iter()
            .find(|s| s.host.eq_ignore_ascii_case(host) && !s.device.is_empty())
            .map(|s| s.device.clone())
            .or_else(|| state.shares.found.iter().find(|f| f.host.eq_ignore_ascii_case(host)).map(|f| f.name.clone()))
            .unwrap_or_else(|| host.to_string())
    }

    fn scan_shares(self: &Rc<Self>) {
        let candidates: Vec<shares::Candidate> = {
            let mut state = self.state.borrow_mut();
            if state.shares.scanning {
                return;
            }
            let Some(status) = state.status.as_ref() else { return };
            let list = status
                .peers
                .iter()
                .filter(|p| p.online && !p.mullvad)
                .filter_map(|p| Some(shares::Candidate { host: p.ipv4()?.to_string(), name: p.display_name() }))
                .collect();
            state.shares.scanning = true;
            list
        };
        self.render();
        let tools = self.tools.clone();
        thread::spawn(move || {
            let mut found = shares::scan(&tools, &candidates);
            found.sort_by_key(|f| f.name.to_lowercase());
            post(move |c| {
                {
                    let mut state = c.state.borrow_mut();
                    state.shares.found = found;
                    state.shares.scanning = false;
                    state.shares.scanned = true;
                }
                c.render();
            });
        });
    }

    /// Connects a share; `ask` when the user clicked, so the OS may prompt.
    fn connect_share(self: &Rc<Self>, key: &str, ask: bool) {
        let Some(share) = self.share_ref(key) else {
            self.notify(2, "That share isn't in the list any more.");
            return;
        };
        {
            let mut state = self.state.borrow_mut();
            if !state.shares.working.insert(key.to_string()) {
                return;
            }
            if ask {
                state.shares.reconnect.release(key);
            }
            state.shares.notes.remove(key);
        }
        self.render();
        let tools = self.tools.clone();
        let key = key.to_string();
        thread::spawn(move || {
            let result = shares::connect(&tools, &share, ask);
            post(move |c| c.on_share_connected(key, share, ask, result));
        });
    }

    fn on_share_connected(self: &Rc<Self>, key: String, share: ShareRef, ask: bool, result: shares::Connected) {
        let os = self.tools.os;
        let name = share.share.clone();
        {
            let mut state = self.state.borrow_mut();
            let view = &mut state.shares;
            view.working.remove(&key);
            match &result.outcome {
                Outcome::Connected(_) => {
                    view.reconnect.succeeded(&key);
                }
                Outcome::Requested => {
                    view.requested.insert(key.clone(), Instant::now());
                }
                Outcome::NeedsSignIn(_) => {
                    view.reconnect.hold(&key, "sign-in");
                    let text = if result.handed_off {
                        format!("Sign in where {} asks, and tick Remember.", os.file_manager())
                    } else {
                        "Needs a sign-in. Click Connect.".to_string()
                    };
                    view.notes.insert(key.clone(), (3, text));
                }
                Outcome::Unsupported(_) => {
                    view.reconnect.hold(&key, "unsupported");
                    view.notes.insert(
                        key.clone(),
                        (4, "This system can't mount SMB shares; install gvfs-smb (gvfs-backends on Ubuntu).".to_string()),
                    );
                }
                Outcome::Failed(msg) => {
                    view.notes.insert(key.clone(), (4, msg.clone()));
                }
            }
        }
        if ask {
            match &result.outcome {
                Outcome::Connected(drive) if !drive.is_empty() => self.notify(1, format!("Connected {name} as {drive}.")),
                Outcome::Connected(_) => self.notify(1, format!("Connected {name}.")),
                Outcome::Requested => self.notify(
                    1,
                    format!("Asked Finder to connect {name}. If it asks for a password, tick Remember in Keychain."),
                ),
                Outcome::NeedsSignIn(_) if result.handed_off => self.notify(
                    1,
                    format!(
                        "{} has no saved sign-in for {name}, so {} is asking for one. Tick Remember, so Tailgate can reconnect it by itself next time.",
                        capitalize(os.keychain()),
                        os.file_manager()
                    ),
                ),
                Outcome::NeedsSignIn(msg) => self.notify(2, format!("{name} needs a sign-in, and {} didn't open: {msg}", os.file_manager())),
                Outcome::Unsupported(msg) if result.handed_off => self.notify(
                    2,
                    format!("Opened {name} in your file manager. Tailgate can't keep it connected here ({msg}); installing gvfs-smb fixes that."),
                ),
                Outcome::Unsupported(msg) => self.notify(2, format!("Couldn't connect {name}: {msg}")),
                Outcome::Failed(msg) => self.notify(2, format!("Couldn't connect {name}. {msg}")),
            }
        }
        self.render();
        self.wake_poll(false);
    }

    fn disconnect_share(self: &Rc<Self>, key: &str) {
        let Some(mounted) = self.mounted_for(key) else { return };
        let name = self.share_parts(key).map_or_else(|| mounted.share.clone(), |(_, share)| share);
        if !self.state.borrow_mut().shares.working.insert(key.to_string()) {
            return;
        }
        self.render();
        let tools = self.tools.clone();
        let key = key.to_string();
        thread::spawn(move || {
            let result = shares::disconnect(&tools, &mounted);
            post(move |c| {
                {
                    let mut state = c.state.borrow_mut();
                    state.shares.working.remove(&key);
                    if result.is_ok() {
                        // Disconnected on purpose, so don't put it back by itself.
                        state.shares.reconnect.hold(&key, "disconnected");
                        state.shares.notes.remove(&key);
                    }
                }
                match result {
                    Ok(()) => c.notify(1, format!("Disconnected {name}. It stays disconnected until you click Connect.")),
                    Err(e) => c.notify(2, format!("Couldn't disconnect {name}. {e}")),
                }
                c.render();
                c.wake_poll(false);
            });
        });
    }

    fn open_share(self: &Rc<Self>, key: &str) {
        let Some(mounted) = self.mounted_for(key) else { return };
        let tools = self.tools.clone();
        thread::spawn(move || {
            if let Err(e) = shares::open(&tools, &mounted) {
                post(move |c| c.notify(2, format!("Couldn't open {}. {e}", mounted.share)));
            }
        });
    }

    fn toggle_share_login(self: &Rc<Self>, key: &str) {
        let existing = self.state.borrow().saved.share(key).cloned();
        let share = match existing {
            Some(mut s) => {
                s.at_login = !s.at_login;
                s
            }
            None => {
                let Some((host, share)) = self.share_parts(key) else { return };
                SavedShare { device: self.device_name(&host), host, share, user: String::new(), at_login: true }
            }
        };
        let on = share.at_login;
        let name = share.share.clone();
        self.state.borrow_mut().saved.upsert_share(share);
        self.save();
        let launch = self.state.borrow().shares.launch_at_login;
        if on && !launch {
            self.notify(1, format!("{name} will connect when Tailgate starts. Turn on Open Tailgate at login to have that happen when you sign in."));
        }
        self.render();
    }

    fn forget_share(self: &Rc<Self>, key: &str) {
        let removed = self.state.borrow_mut().saved.forget_share(key);
        if removed {
            self.state.borrow_mut().shares.notes.remove(key);
            self.save();
            self.render();
        }
    }

    fn add_share_submit(self: &Rc<Self>) {
        let Some(ui) = self.ui() else { return };
        let app = ui.global::<AppGlobal>();
        let share = match ShareRef::new(&app.get_add_share_host(), &app.get_add_share_name(), &app.get_add_share_user()) {
            Ok(share) => share,
            Err(e) => {
                app.set_add_share_error(e.into());
                return;
            }
        };
        let device = self.device_name(&share.host);
        let at_login = self.state.borrow().saved.share(&share.key()).is_some_and(|s| s.at_login);
        self.state.borrow_mut().saved.upsert_share(SavedShare {
            host: share.host.clone(),
            share: share.share.clone(),
            user: share.user.clone(),
            device,
            at_login,
        });
        self.save();
        app.set_add_share_open(false);
        app.set_add_share_error("".into());
        self.notify(1, format!("Added {}. Click Connect to use it.", share.share));
        self.render();
    }

    fn toggle_launch_at_login(self: &Rc<Self>) {
        if self.client.read_only {
            self.notify(1, "Read-only mode: didn't change the login items.");
            return;
        }
        let want = !autostart::is_enabled();
        match autostart::set_enabled(want) {
            Ok(()) => self.notify(
                1,
                if want { "Tailgate will open, minimized, when you sign in." } else { "Tailgate won't open at login any more." },
            ),
            Err(e) => self.notify(2, format!("Couldn't change that. {e}")),
        }
        self.state.borrow_mut().shares.launch_at_login = autostart::is_enabled();
        self.render();
    }

    /// Notices Tailscale coming back, sleep and hosts returning, and retries
    /// "Connect at login" shares as the scheduler says.
    fn after_poll_shares(self: &Rc<Self>, mounted: Vec<Mounted>) {
        let now = Instant::now();
        let wall = SystemTime::now();
        let at_login: Vec<SavedShare> = self.state.borrow().saved.shares.iter().filter(|s| s.at_login).cloned().collect();
        // Aliases need the state borrow, so work them out first.
        let aliases: Vec<Vec<String>> = at_login.iter().map(|s| self.host_aliases(&s.host)).collect();
        let due = {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            let running = matches!(state.status.as_ref().map(|s| &s.backend), Some(Backend::Running));
            let view = &mut state.shares;
            view.mounted = mounted;
            let keys: Vec<String> = at_login.iter().map(SavedShare::key).collect();

            // Tailgate starting with Tailscale up counts as coming back too.
            let came_back = running && view.was_running != Some(true);
            let woke = view.last_poll.is_some_and(|(w, m)| {
                shares::slept(wall.duration_since(w).unwrap_or_default(), now.duration_since(m))
            });
            if came_back || woke {
                view.reconnect.trigger(now, keys.iter().map(String::as_str));
            }
            view.was_running = Some(running);
            view.last_poll = Some((wall, now));

            let mut wants = Vec::new();
            for (i, share) in at_login.iter().enumerate() {
                let peer = state.status.as_ref().and_then(|s| peer_for(s, &share.host));
                let online = peer.map(|p| p.online);
                let key = &keys[i];
                if let Some(online) = online {
                    if online && view.online.get(key) == Some(&false) {
                        view.reconnect.trigger(now, [key.as_str()]);
                    }
                    view.online.insert(key.clone(), online);
                }
                let connected = view.mounted.iter().any(|m| m.is(&share.share, &aliases[i]));
                if connected {
                    view.requested.remove(key);
                }
                let busy = view.working.contains(key) || view.requested.contains_key(key);
                wants.push((key.clone(), connected || busy, running && online.unwrap_or(true)));
            }
            let wants: Vec<Want> =
                wants.iter().map(|(key, connected, reachable)| Want { key, connected: *connected, reachable: *reachable }).collect();
            let due = view.reconnect.due(now, &wants);

            // Finder never mounted it: stop saying it's on its way.
            let stale: Vec<String> =
                view.requested.iter().filter(|(_, at)| now.duration_since(**at) > FINDER_WAIT).map(|(k, _)| k.clone()).collect();
            for key in stale {
                view.requested.remove(&key);
                view.notes.insert(key, (0, "Not connected. If Finder asked for a password, finish there.".into()));
            }
            // Mounted shares need no notes.
            let mounted_keys: Vec<String> =
                view.notes.keys().filter(|k| view.mounted.iter().any(|m| shares::share_key(&m.host, &m.share) == **k)).cloned().collect();
            for key in mounted_keys {
                view.notes.remove(&key);
            }
            due
        };
        for key in due {
            self.connect_share(&key, false);
        }
    }

    fn render_shares(&self, app: &crate::App) {
        let state = self.state.borrow();
        let status = state.status.as_ref();
        let view = &state.shares;
        let os = self.tools.os;
        let running = matches!(status.map(|s| &s.backend), Some(Backend::Running));

        // Devices to show: scanned, saved, or holding a connected share.
        struct Device {
            name: String,
            host: String,
            online: Option<bool>,
            listed: Option<bool>,
            shares: Vec<(String, String)>,
        }
        let mut devices: BTreeMap<String, Device> = BTreeMap::new();
        let device_key = |host: &str| -> String {
            match status.and_then(|s| peer_for(s, host)) {
                Some(p) => p.id.clone(),
                None => host.to_lowercase(),
            }
        };
        let mut add = |host: &str, name: &str, share: Option<&str>, listed: Option<bool>| {
            let peer = status.and_then(|s| peer_for(s, host));
            let entry = devices.entry(device_key(host)).or_insert_with(|| Device {
                name: peer.map(|p| p.display_name()).unwrap_or_else(|| if name.is_empty() { host.to_string() } else { name.to_string() }),
                host: peer.and_then(|p| p.ipv4().map(String::from)).unwrap_or_else(|| host.to_string()),
                online: peer.map(|p| p.online),
                listed: None,
                shares: Vec::new(),
            });
            if listed.is_some() {
                entry.listed = listed;
            }
            if let Some(share) = share {
                let key = shares::share_key(host, share);
                let same = |(k, s): &(String, String)| *k == key || s.eq_ignore_ascii_case(share);
                if !entry.shares.iter().any(same) {
                    entry.shares.push((key, share.to_string()));
                }
            }
        };
        for s in &state.saved.shares {
            add(&s.host, &s.device, Some(&s.share), None);
        }
        for f in &view.found {
            add(&f.host, &f.name, None, Some(f.shares.is_some()));
            for share in f.shares.iter().flatten() {
                add(&f.host, &f.name, Some(share), None);
            }
        }
        // Shares connected some other way (Finder, a login script) on tailnet devices.
        for m in &view.mounted {
            if status.and_then(|s| peer_for(s, &m.host)).is_some() {
                add(&m.host, "", Some(&m.share), None);
            }
        }

        let mut list: Vec<Device> = devices.into_values().collect();
        list.sort_by_key(|d| (d.online == Some(false), d.name.to_lowercase()));
        let mut rows = Vec::new();
        for mut d in list {
            d.shares.sort_by_key(|(_, s)| s.to_lowercase());
            let mut detail = d.host.clone();
            if d.online == Some(false) {
                detail.push_str(" · offline");
            }
            rows.push(ShareRow { kind: 0, title: d.name.clone().into(), detail: detail.into(), host: d.host.clone().into(), ..ShareRow::default() });
            if d.shares.is_empty() && d.listed == Some(false) {
                rows.push(ShareRow {
                    kind: 2,
                    detail: "File sharing answers here, but its share names need a sign-in to list. Add one by name.".into(),
                    ..ShareRow::default()
                });
            }
            for (key, share) in d.shares {
                let saved = state.saved.share(&key);
                let aliases = {
                    let mut a = vec![d.host.clone()];
                    if let Some(p) = status.and_then(|s| peer_for(s, &d.host)) {
                        a.extend(p.ips.iter().cloned());
                        a.push(p.host.clone());
                        a.push(p.fqdn().to_string());
                        a.push(p.display_name());
                    }
                    if let Some(s) = saved {
                        a.push(s.host.clone());
                    }
                    a
                };
                let mounted = view.mounted.iter().find(|m| m.is(&share, &aliases));
                let at_login = saved.is_some_and(|s| s.at_login);
                let discovered = view.found.iter().any(|f| f.shares.iter().flatten().any(|n| shares::share_key(&f.host, n) == key));
                let (state_code, text) = if view.working.contains(&key) {
                    (2, if mounted.is_some() { "Disconnecting…".to_string() } else { "Connecting…".to_string() })
                } else if let Some(m) = mounted {
                    (1, connected_text(os, m))
                } else if view.requested.contains_key(&key) {
                    (2, "Waiting for Finder…".to_string())
                } else if d.online == Some(false) {
                    (5, if at_login { "Device offline · reconnects when it's back".into() } else { "Device offline".into() })
                } else if let Some((code, note)) = view.notes.get(&key) {
                    (*code, note.clone())
                } else if at_login && !running {
                    (0, "Not connected · reconnects when Tailscale is back".into())
                } else if at_login && view.reconnect.gave_up(&key) {
                    (0, "Couldn't reconnect. Tries again when Tailscale reconnects.".into())
                } else if at_login && view.reconnect.active(&key) {
                    (0, "Reconnecting…".into())
                } else {
                    (0, "Not connected".into())
                };
                let user = saved.map(|s| s.user.as_str()).unwrap_or("");
                let title = if user.is_empty() { share.clone() } else { format!("{share}  ·  as {user}") };
                rows.push(ShareRow {
                    kind: 1,
                    key: key.clone().into(),
                    title: title.into(),
                    detail: text.into(),
                    host: d.host.clone().into(),
                    state: state_code,
                    at_login,
                    can_disconnect: mounted.is_some_and(|m| m.kind != MountKind::Cifs),
                    can_forget: saved.is_some() && !discovered && mounted.is_none(),
                });
            }
        }

        let online = status.map_or(0, |s| s.peers.iter().filter(|p| p.online && !p.mullvad).count());
        app.set_shares_scanning(view.scanning);
        app.set_shares_summary(
            if view.scanning {
                format!("Looking for file shares on {online} online devices…")
            } else if view.scanned {
                let with = view.found.len();
                format!("File sharing on {with} of {online} online devices")
            } else {
                String::new()
            }
            .into(),
        );
        app.set_shares_empty(
            if !rows.is_empty() {
                String::new()
            } else if status.is_none() {
                "No list until Tailscale answers.".into()
            } else if view.scanning {
                "Looking for file shares…".into()
            } else if view.scanned {
                "No device on your tailnet answers on the file-sharing port (445). If you know a share's name, add it.".into()
            } else {
                "Press Scan again to look for file shares on your devices.".into()
            }
            .into(),
        );
        app.set_shares_footer(
            format!(
                "Tailgate never sees your passwords: connecting uses the sign-ins saved in {}. Shares marked At login connect when Tailgate starts and again after sleep or when Tailscale reconnects.",
                os.keychain()
            )
            .into(),
        );
        app.set_launch_at_login(view.launch_at_login);
        let any_at_login = state.saved.shares.iter().any(|s| s.at_login);
        app.set_launch_at_login_hint(
            if view.launch_at_login {
                "Opens minimized when you sign in, and connects the shares marked At login."
            } else if any_at_login {
                "Turn on so the shares marked At login connect when you sign in."
            } else {
                "Opens minimized when you sign in to this computer."
            }
            .into(),
        );
        set_model(&self.share_rows, rows);
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
        app.set_setup_step(state.setup);
        app.set_setup_operator(!self.user.is_empty() && prefs.and_then(|p| p.operator.as_deref()) == Some(self.user.as_str()));
        app.set_setup_user(self.user.clone().into());
        if state.setup == 1 || state.setup == 2 {
            app.set_title(if state.setup == 1 { "Tailscale isn't installed" } else { "Tailscale isn't running" }.into());
            app.set_pill("Setup".into());
            app.set_detail("Follow the steps on this page. Tailgate notices by itself when it's done.".into());
        }
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
        drop(state);
        self.render_shares(&app);
    }
}

/// The tailnet device (or this machine) a host names, by IP or name.
fn peer_for<'a>(status: &'a Status, host: &str) -> Option<&'a Peer> {
    let host = host.trim_end_matches('.');
    std::iter::once(&status.self_peer).chain(status.peers.iter()).find(|p| {
        p.ips.iter().any(|ip| ip.eq_ignore_ascii_case(host))
            || (!p.host.is_empty() && p.host.eq_ignore_ascii_case(host))
            || (!p.dns.is_empty() && p.fqdn().eq_ignore_ascii_case(host))
            || p.display_name().eq_ignore_ascii_case(host)
    })
}

fn connected_text(os: shares::Os, m: &Mounted) -> String {
    match (os, m.location.as_str()) {
        (_, "") if os == shares::Os::Windows => "Connected".into(),
        // gvfs mounts show in the Files/Nautilus sidebar (and under ~/.gvfs-style fuse paths).
        (_, _) if m.kind == MountKind::Gvfs => "Connected · in your file manager".into(),
        (_, "") => "Connected".into(),
        (shares::Os::Windows, drive) => format!("Connected as drive {drive}"),
        (_, path) => format!("Connected at {path}"),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
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

fn spawn_poller(client: Client, tools: Tools, generation: Arc<AtomicU64>, labels: BTreeMap<u32, String>) -> Sender<bool> {
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
            // Cheap (a file read, or one quick command), and it's how a share
            // dropped by sleep or a network change gets noticed.
            let mounted = shares::list_mounted(&tools);
            post(move |c| c.on_poll(PollResult { generation: generation_now, status, prefs, serve, mounted }));

            match rx.recv_timeout(POLL_EVERY) {
                Ok(with_serve) => force_serve |= with_serve,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    tx
}


/// `minimized`: started at login, so stay out of the way.
pub fn run(read_only: bool, minimized: bool) -> Result<(), slint::PlatformError> {
    let client = Client::from_env(read_only);
    let tools = Tools::system(read_only);
    let state_path = store::default_path();
    let saved = store::load(&state_path);

    let ui = AppWindow::new()?;
    // Ties the window to tailgate.desktop. Needs the backend `new` just started.
    #[cfg(target_os = "linux")]
    slint::set_xdg_app_id("tailgate")?;
    ui.global::<Theme>().set_dark(saved.dark);

    let generation = Arc::new(AtomicU64::new(0));
    let labels = store::load_labels(&state_path.with_file_name("labels.json"));
    let poll = spawn_poller(client.clone(), tools.clone(), generation.clone(), labels);

    let banner = match (read_only, client.custom_bin) {
        (true, true) => format!("Read-only mode, using a test binary: {}", client.bin.display()),
        (true, false) => "Read-only mode: nothing here can change Tailscale.".to_string(),
        (false, true) => format!("Using a test binary: {}", client.bin.display()),
        (false, false) => client.variant.note().unwrap_or_default().to_string(),
    };
    let banner_action = if client.variant == cli::Variant::NotFound { "Get Tailscale" } else { "" };
    let setup_install = if cli::on_path("curl") {
        "curl -fsSL https://tailscale.com/install.sh | sh"
    } else {
        // Stock Ubuntu desktops can lack curl; this still runs the same official script.
        "sudo apt install -y curl && curl -fsSL https://tailscale.com/install.sh | sh"
    };

    let controller = Rc::new(Controller {
        ui: ui.as_weak(),
        user: cli::current_user().unwrap_or_default(),
        client,
        tools,
        state_path,
        state: RefCell::new(State {
            saved,
            shares: SharesView { launch_at_login: autostart::is_enabled(), ..SharesView::default() },
            ..State::default()
        }),
        nodes: Rc::new(VecModel::default()),
        devices: Rc::new(VecModel::default()),
        serve_rows: Rc::new(VecModel::default()),
        share_rows: Rc::new(VecModel::default()),
        notice_timer: slint::Timer::default(),
        poll,
        generation,
    });
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));

    let app = ui.global::<AppGlobal>();
    app.set_banner(banner.into());
    app.set_banner_action(banner_action.into());
    app.set_setup_install(setup_install.into());
    app.on_banner_clicked(|| with(|c| c.open(cli::DOWNLOAD_URL)));
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
    app.set_share_rows(ModelRc::from(controller.share_rows.clone()));

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
                // `tailscale up` waits for the login, so the page opens from the next poll.
                let needs_login = c.state.borrow().setup == 3;
                c.state.borrow_mut().login_requested = needs_login;
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
    app.on_tab_changed(|tab| {
        with(move |c| {
            c.wake_poll(true);
            let scanned = c.state.borrow().shares.scanned;
            if tab == c.shares_tab() && !scanned {
                c.scan_shares();
            }
        })
    });
    app.on_scan_shares(|| with(|c| c.scan_shares()));
    app.on_connect_share(|key| {
        with(move |c| {
            if !c.modal_open() {
                c.connect_share(&key, true);
            }
        })
    });
    app.on_disconnect_share(|key| {
        with(move |c| {
            if !c.modal_open() {
                c.disconnect_share(&key);
            }
        })
    });
    app.on_open_share(|key| with(move |c| c.open_share(&key)));
    app.on_toggle_share_login(|key| with(move |c| c.toggle_share_login(&key)));
    app.on_forget_share(|key| with(move |c| c.forget_share(&key)));
    app.on_add_share_for(|host| {
        with(move |c| {
            if let Some(ui) = c.ui() {
                let app = ui.global::<AppGlobal>();
                app.set_add_share_host(host);
                app.set_add_share_name("".into());
                app.set_add_share_user("".into());
                app.set_add_share_error("".into());
                app.set_add_share_open(true);
            }
        })
    });
    app.on_add_share_submit(|| with(|c| c.add_share_submit()));
    app.on_add_share_cancel(|| {
        with(|c| {
            if let Some(ui) = c.ui() {
                ui.global::<AppGlobal>().set_add_share_open(false);
            }
        })
    });
    app.on_toggle_launch_at_login(|| with(|c| c.toggle_launch_at_login()));

    // Started at login: minimize once the window exists. Shares marked At
    // login connect from the first poll that finds Tailscale running.
    let minimize = slint::Timer::default();
    if minimized {
        let weak = ui.as_weak();
        minimize.start(slint::TimerMode::SingleShot, Duration::from_millis(400), move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
    }

    controller.render();
    let result = ui.run();
    CONTROLLER.with(|c| *c.borrow_mut() = None);
    result
}

/// What to try when Tailscale on Windows or macOS refuses a change.
fn denied_hint(variant: cli::Variant) -> &'static str {
    match variant {
        cli::Variant::Windows => {
            "On Windows, Tailscale takes changes from the account that's signed in to the Tailscale app. \
             Check the Tailscale icon in the taskbar, then try again."
        }
        cli::Variant::MacAppStore => {
            "The App Store version only takes changes while its app is running. Open Tailscale from the \
             menu bar and try again, or switch to the Standalone version from tailscale.com/download."
        }
        _ => "Try the same change in the Tailscale app.",
    }
}

/// Callbacks run on the UI thread, where the controller is already reachable.
fn with(f: impl FnOnce(&Rc<Controller>)) {
    let controller = CONTROLLER.with(|c| c.borrow().clone());
    if let Some(controller) = controller {
        f(&controller);
    }
}
