#![cfg_attr(windows, windows_subsystem = "windows")]
//! Weft desktop app, drawn by Slint without a web engine.

mod daemon;
mod deploy;
mod instance;
mod notify;
mod picture;
mod repair;
mod settings;
mod shot;
mod system;
mod update;
mod view;

mod ui {
    #![allow(clippy::all, clippy::todo)]
    slint::include_modules!();
}

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use weft_i18n::{Language, Localizer};
use weft_ipc::{Connection, NetworkStatus, Request, Response, Role, ServerStatus, Status};

use daemon::DaemonState;
use ui::*;

const POLL: Duration = Duration::from_millis(1500);
const STARTUP_GRACE: Duration = Duration::from_secs(4);
const CONNECT_WAIT: Duration = Duration::from_secs(20);
const EXPIRY: [(&str, Option<u64>); 5] = [
    ("gui-expiry-never", None),
    ("gui-expiry-hour", Some(3600)),
    ("gui-expiry-day", Some(86_400)),
    ("gui-expiry-week", Some(7 * 86_400)),
    ("gui-expiry-month", Some(30 * 86_400)),
];
const DEPLOY_STEPS: [&str; 5] = ["check", "download", "configure", "firewall", "start"];
/// Mute lengths offered for a member: fixed ones, then a custom one and forever.
const MUTES: [(&str, u64); 4] =
    [("gui-mute-5m", 300), ("gui-mute-30m", 1800), ("gui-mute-1h", 3600), ("gui-mute-5h", 5 * 3600)];
const MUTE_UNITS: [(&str, u64); 3] = [("gui-unit-minutes", 60), ("gui-unit-hours", 3600), ("gui-unit-days", 86_400)];
/// How long a refresh waits for answers before showing them.
const REFRESH_WAIT: Duration = Duration::from_millis(1200);

struct Options {
    hidden: bool,
    demo: bool,
    shot: Option<PathBuf>,
    language: Option<Language>,
    dark: Option<bool>,
    dialog: Option<String>,
    clicks: Vec<(f32, f32)>,
}

fn options() -> Options {
    let mut options = Options {
        hidden: false,
        demo: false,
        shot: None,
        language: None,
        dark: None,
        dialog: None,
        clicks: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hidden" => options.hidden = true,
            "--demo" => options.demo = true,
            "--dark" => options.dark = Some(true),
            "--light" => options.dark = Some(false),
            "--screenshot" => options.shot = args.next().map(PathBuf::from),
            "--lang" => options.language = args.next().as_deref().and_then(Language::from_code),
            "--dialog" => options.dialog = args.next(),
            "--click" => {
                let point = args.next().unwrap_or_default();
                if let Some((x, y)) = point.split_once(',')
                    && let (Ok(x), Ok(y)) = (x.parse(), y.parse())
                {
                    options.clicks.push((x, y));
                }
            }
            _ => {}
        }
    }
    options
}

/// What the open dialog works on.
#[derive(Default)]
struct Context {
    network: Option<(ServerStatus, NetworkStatus)>,
    member: Option<(String, String, bool)>,
    server: Option<ServerStatus>,
    choices: Vec<String>,
}

struct App {
    ui: slint::Weak<AppWindow>,
    tray: RefCell<Option<WeftTray>>,
    l: Arc<Localizer>,
    store: RefCell<settings::Store>,
    networks: RefCell<view::Networks>,
    status: RefCell<Option<Status>>,
    /// Pictures of devices by id, and the ids being fetched.
    pictures: RefCell<view::Pictures>,
    fetching: RefCell<HashSet<String>>,
    context: RefCell<Context>,
    notifier: RefCell<notify::Notifier>,
    problem: Cell<DaemonState>,
    started: Instant,
    polling: Cell<bool>,
    demo: bool,
    toast: Timer,
    invites: Rc<VecModel<InviteRow>>,
    devices: Rc<VecModel<DeviceRow>>,
    servers: Rc<VecModel<ServerRow>>,
}

fn main() -> Result<(), slint::PlatformError> {
    let options = options();
    if options.shot.is_some() {
        shot::install(440, 680);
    }
    let store = settings::Store::load();
    let language = options
        .language
        .or_else(|| store.current.language.as_deref().and_then(Language::from_code))
        .unwrap_or(Language::English);
    let l = Arc::new(Localizer::new(language));
    // Matches weft.desktop, so docks and launchers show the right icon.
    let _ = slint::set_xdg_app_id("weft");
    let ui = AppWindow::new()?;

    if options.shot.is_none() && !options.demo {
        let weak = ui.as_weak();
        let first = instance::claim(move || {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    reveal(&ui);
                }
            });
        });
        if !first {
            return Ok(());
        }
    }

    let dark = options.dark.or(store.current.dark);
    let app = Rc::new(App {
        ui: ui.as_weak(),
        tray: RefCell::new(None),
        l,
        store: RefCell::new(store),
        networks: RefCell::new(view::Networks::default()),
        status: RefCell::new(None),
        pictures: RefCell::new(view::Pictures::new()),
        fetching: RefCell::new(HashSet::new()),
        context: RefCell::new(Context::default()),
        notifier: RefCell::new(notify::Notifier::default()),
        problem: Cell::new(DaemonState::Ok),
        started: Instant::now(),
        polling: Cell::new(false),
        demo: options.demo || options.shot.is_some(),
        toast: Timer::default(),
        invites: Rc::new(VecModel::default()),
        devices: Rc::new(VecModel::default()),
        servers: Rc::new(VecModel::default()),
    });
    app.wire(&ui);
    if let Some(dark) = dark {
        ui.global::<Theme>().set_dark(dark);
    }

    let poller = Timer::default();
    let updates = Timer::default();
    if app.demo {
        app.show_status(view::demo());
    } else {
        app.tray();
        app.poll();
        let polled = app.clone();
        poller.start(TimerMode::Repeated, POLL, move || polled.poll());
        app.check_updates();
        let checked = app.clone();
        updates.start(TimerMode::Repeated, update::EVERY, move || checked.check_updates());
    }

    if let Some(path) = options.shot {
        if let Some(dialog) = options.dialog {
            app.open_by_name(&dialog);
        }
        ui.show()?;
        for (x, y) in options.clicks {
            shot::click(x, y);
        }
        return shot::save(&ui, &path);
    }
    ui.window().on_close_requested(|| CloseRequestResponse::HideWindow);
    if !options.hidden {
        ui.show()?;
    }
    slint::run_event_loop_until_quit()
}

fn reveal(ui: &AppWindow) {
    let _ = ui.show();
    ui.window().set_minimized(false);
}

impl App {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("the window lives as long as the app")
    }

    fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        let _ = slint::spawn_local(task);
    }

    async fn send(&self, request: Request, server: Option<String>) -> Result<Response, String> {
        daemon::send(&self.l, request, server).await
    }

    fn tr(&self, id: &str) -> String {
        self.l.tr(id)
    }

    fn tr_args(&self, id: &str, args: &[(&str, &str)]) -> String {
        self.l.tr_args(id, args)
    }

    /// Connects every callback of the window to the app.
    fn wire(self: &Rc<Self>, ui: &AppWindow) {
        let i18n = ui.global::<I18n>();
        let l = self.l.clone();
        i18n.on_translate(move |id| l.tr(&id).into());
        let l = self.l.clone();
        i18n.on_translate_with(move |id, name, value| l.tr_args(&id, &[(&name, &value)]).into());
        i18n.set_rtl(self.l.language().is_rtl());
        ui.global::<Utils>().on_is_link(|text| text.trim().to_lowercase().starts_with("weft://"));

        ui.set_networks(ModelRc::from(self.networks.borrow().cards.clone()));
        ui.set_invites(ModelRc::from(self.invites.clone()));
        ui.set_devices(ModelRc::from(self.devices.clone()));
        ui.set_servers(ModelRc::from(self.servers.clone()));
        ui.global::<Chooser>().set_steps(ModelRc::new(VecModel::<Step>::default()));

        macro_rules! on {
            ($app:ident, $setter:ident, || $body:expr) => {{
                let $app = self.clone();
                ui.$setter(move || {
                    let $app = &$app;
                    $body
                });
            }};
            ($app:ident, $setter:ident, |$($arg:ident),*| $body:expr) => {{
                let $app = self.clone();
                ui.$setter(move |$($arg),*| {
                    let $app = &$app;
                    $body
                });
            }};
        }

        on!(app, on_power, || app.power());
        on!(app, on_toggle_theme, || app.toggle_theme());
        on!(app, on_open_settings, || app.open_settings());
        on!(app, on_open_diagnostics, || app.open_diagnostics());
        on!(app, on_create, || app.open_create());
        on!(app, on_join, || app.open_join());
        on!(app, on_copy, |text| app.copy(&text));
        on!(app, on_toggle_network, |index| app.toggle_network(index as usize));
        on!(app, on_network_invites, |index| app.with_network(index as usize, App::open_invites));
        on!(app, on_network_requests, |index| app.with_network(index as usize, |app| app.open_devices("requests")));
        on!(app, on_network_menu, |index| app.with_network(index as usize, App::open_network_menu));
        on!(app, on_member_menu, |network, member| app.open_member(network as usize, member as usize));
        on!(app, on_member_ping, |network, member| app.ping_member(network as usize, member as usize));
        on!(app, on_refresh, || app.refresh_all());
        on!(app, on_fix_problem, || app.fix_problem());
        on!(app, on_download_update, || app.download_update());
        on!(app, on_nickname_changed, |name| app.rename(name.to_string(), false));
        on!(app, on_dialog_leave, || app.leave());
        on!(app, on_create_submit, |name, password, repeat| app.create_network(
            name.to_string(),
            password.to_string(),
            repeat.to_string()
        ));
        on!(app, on_join_submit, |target, password| app.join_network(target.to_string(), password.to_string()));
        on!(app, on_settings_save, |name| app.rename(name.to_string(), true));
        on!(app, on_language_chosen, |index| app.choose_language(index as usize));
        on!(app, on_open_servers, || app.open_servers());
        on!(app, on_notifications_toggled, |on| {
            app.store.borrow_mut().update(|settings| settings.notifications = on);
        });
        on!(app, on_open_notifications, || app.open_notifications());
        on!(app, on_notify_kind_toggled, |kind, on| app.toggle_notify(&kind, on));
        on!(app, on_pick_picture, || app.pick_picture());
        on!(app, on_remove_picture, || app.set_picture(None));
        on!(app, on_updates_toggled, |on| {
            app.store.borrow_mut().update(|settings| settings.updates = on);
            app.check_updates();
        });
        on!(app, on_reset_app, || app.reset());
        on!(app, on_server_menu, |index| app.open_server_menu(index as usize));
        on!(app, on_add_server, || app.open_add_server());
        on!(app, on_add_server_submit, || app.add_server());
        on!(app, on_host_settings, || app.open_host());
        on!(app, on_copy_server_link, || app.copy_server_link());
        on!(app, on_open_panel, || app.open_panel());
        on!(app, on_copy_panel_password, || app.copy_panel_password());
        on!(app, on_remove_server, || app.remove_server());
        on!(app, on_host_save, |address, port| app.save_host(address.to_string(), port.to_string()));
        on!(app, on_invite_create, |uses, expiry| app.create_invite(uses.to_string(), expiry as usize));
        on!(app, on_invite_revoke, |code| app.revoke_invite(code.to_string()));
        on!(app, on_device_act, |command, key| app.device_action(command.to_string(), key.to_string()));
        on!(app, on_network_configure, |key, on| app.configure(key.to_string(), on));
        on!(app, on_network_password, |password| app.change_password(password.to_string()));
        on!(app, on_network_rename, |name| app.rename_network(name.to_string()));
        on!(app, on_network_delete, || app.delete_network());
        on!(app, on_network_pick, |item| app.network_pick(&item));
        on!(app, on_member_act, |command| app.member_action(command.to_string()));
        on!(app, on_member_mute, |duration, amount, unit| app.mute_member(
            duration as usize,
            amount.to_string(),
            unit as usize
        ));
        on!(app, on_report_refresh, || app.load_report());
        on!(app, on_report_save, || app.save_report());
    }

    fn tray(self: &Rc<Self>) {
        let Ok(tray) = WeftTray::new() else { return };
        let app = self.clone();
        tray.on_open(move || reveal(&app.ui()));
        let app = self.clone();
        tray.on_connect(move || app.request_then_poll(Request::Up { link: None, nickname: None }));
        let app = self.clone();
        tray.on_disconnect(move || app.request_then_poll(Request::Down));
        tray.on_quit(|| {
            let _ = slint::quit_event_loop();
        });
        *self.tray.borrow_mut() = Some(tray);
        self.tray_texts();
    }

    fn tray_texts(&self) {
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.set_open_text(self.tr("gui-tray-open").into());
            tray.set_connect_text(self.tr("gui-connect").into());
            tray.set_disconnect_text(self.tr("gui-disconnect").into());
            tray.set_quit_text(self.tr("gui-tray-quit").into());
        }
    }

    // Status

    fn poll(self: &Rc<Self>) {
        if self.demo || self.polling.replace(true) {
            return;
        }
        let app = self.clone();
        self.spawn(async move {
            let result = app.send(Request::Status, None).await;
            app.polling.set(false);
            match result {
                Ok(Response::Status(status)) => app.show_status(status),
                Ok(_) => {}
                Err(error) => app.show_problem(error).await,
            }
        });
    }

    /// Fetches the status now and shows it, for flows that wait on the daemon.
    async fn refresh(self: &Rc<Self>) -> Option<Status> {
        if let Ok(Response::Status(status)) = self.send(Request::Status, None).await {
            self.show_status(status.clone());
            return Some(status);
        }
        None
    }

    fn show_status(self: &Rc<Self>, status: Status) {
        let ui = self.ui();
        let wanted = self.store.borrow().current.clone();
        let wanted = (wanted.notifications && !self.demo).then_some(&wanted.notify);
        self.notifier.borrow_mut().observe(&status, wanted, &self.l);
        let welcome = status.servers.is_empty() && status.host.is_none();
        if welcome && ui.get_phase() != "welcome" {
            ui.set_welcome_nickname(status.nickname.clone().into());
        }
        ui.set_phase(if welcome { "welcome" } else { "main" }.into());
        *self.status.borrow_mut() = Some(status);
        self.render();
        self.fetch_pictures();
        if !self.demo && system::restart_if_replaced(ui.window().is_visible()) {
            let _ = slint::quit_event_loop();
        }
    }

    /// Redraws the main screen from the last status, also after a language change.
    fn render(&self) {
        let ui = self.ui();
        let Some(status) = self.status.borrow().clone() else { return };
        let collapsed: HashSet<String> = self.store.borrow().current.collapsed.iter().cloned().collect();
        let connection = view::overall(&status.servers);
        self.networks.borrow_mut().update(&self.l, &status, &collapsed, &self.pictures.borrow());
        ui.set_nickname(status.nickname.clone().into());
        ui.set_connection(connection.into());
        let address = status.servers.iter().find_map(|server| server.address).map(|a| a.to_string());
        ui.set_address(address.unwrap_or_default().into());
        let (empty, hint) = match connection {
            "connected" => ("gui-no-networks", None),
            "connecting" => ("gui-waiting-server", None),
            _ => ("gui-off-title", Some("gui-off-hint")),
        };
        let refused = status.servers.iter().find_map(|server| Some((server, server.refused?)));
        match refused {
            // A server turned this device away: say why instead of just "off".
            Some((server, failure)) if connection != "connected" => {
                ui.set_empty_text(self.tr(failure.message_id()).into());
                ui.set_empty_hint(view::server_name(&self.l, server).into());
            }
            _ => {
                ui.set_empty_text(self.tr(empty).into());
                ui.set_empty_hint(hint.map(|id| self.tr(id)).unwrap_or_default().into());
            }
        }
        if ui.get_dialog() == "servers" {
            view::sync(&self.servers, view::server_rows(&self.l, &status));
        }
    }

    async fn show_problem(self: &Rc<Self>, error: String) {
        let ui = self.ui();
        if self.started.elapsed() < STARTUP_GRACE && self.status.borrow().is_none() {
            ui.set_phase("starting".into());
            return;
        }
        let state = daemon::state().await;
        self.problem.set(state);
        let (title, text, action) = match state {
            DaemonState::Missing => (self.tr("gui-repair-missing"), self.tr("gui-repair-hint"), "gui-repair"),
            DaemonState::Denied => (self.tr("gui-repair-denied"), self.tr("gui-repair-hint"), "gui-repair"),
            _ => (self.tr("gui-daemon-problem"), error, "gui-retry"),
        };
        if !ui.get_problem_busy() {
            ui.set_problem_title(title.into());
            ui.set_problem_text(text.into());
            ui.set_problem_action(self.tr(action).into());
        }
        ui.set_phase("problem".into());
    }

    fn fix_problem(self: &Rc<Self>) {
        if !matches!(self.problem.get(), DaemonState::Missing | DaemonState::Denied) {
            self.poll();
            return;
        }
        let ui = self.ui();
        ui.set_problem_busy(true);
        ui.set_problem_action(self.tr("gui-repairing").into());
        let app = self.clone();
        self.spawn(async move {
            let result = daemon::background(async { tokio::task::spawn_blocking(repair::run).await })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
            app.ui().set_problem_busy(false);
            match result {
                Ok(()) => app.toast(&app.tr("gui-repaired"), false),
                Err(reason) => app.toast(&app.tr_args("gui-repair-failed", &[("reason", &reason)]), true),
            }
            app.poll();
        });
    }

    // Small actions on the main screen

    fn toast(&self, text: &str, error: bool) {
        let ui = self.ui();
        ui.set_toast_text(text.into());
        ui.set_toast_error(error);
        ui.set_toast_shown(true);
        let weak = self.ui.clone();
        let duration = Duration::from_millis(if error { 5000 } else { 2500 });
        self.toast.start(TimerMode::SingleShot, duration, move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_toast_shown(false);
            }
        });
    }

    fn copy(&self, text: &str) {
        match system::copy(text) {
            Ok(()) => self.toast(&self.tr("gui-copied"), false),
            Err(error) => self.toast(&error, true),
        }
    }

    /// Asks the servers again and measures every ping, then shows the result.
    fn refresh_all(self: &Rc<Self>) {
        let ui = self.ui();
        if ui.get_refreshing() || self.demo {
            return;
        }
        ui.set_refreshing(true);
        let app = self.clone();
        self.spawn(async move {
            if let Err(error) = app.send(Request::Refresh { address: None }, None).await {
                app.toast(&error, true);
            }
            daemon::background(tokio::time::sleep(REFRESH_WAIT)).await;
            app.refresh().await;
            app.ui().set_refreshing(false);
        });
    }

    /// Measures one member's ping again; the own row stands for the server and refreshes all.
    fn ping_member(self: &Rc<Self>, network: usize, member: usize) {
        let shown = self.networks.borrow().shown.get(network).cloned();
        let Some((_, network)) = shown else { return };
        let Some(address) = member.checked_sub(1).and_then(|index| network.members.get(index)).map(|m| m.address)
        else {
            return self.refresh_all();
        };
        if self.demo {
            return;
        }
        let app = self.clone();
        self.spawn(async move {
            let _ = app.send(Request::Refresh { address: Some(address) }, None).await;
            daemon::background(tokio::time::sleep(REFRESH_WAIT)).await;
            app.refresh().await;
        });
    }

    /// Loads the pictures the status mentions that are not here yet.
    fn fetch_pictures(self: &Rc<Self>) {
        let Some(status) = self.status.borrow().clone() else { return };
        let ids: HashSet<String> = status
            .avatar
            .iter()
            .cloned()
            .chain(
                status
                    .servers
                    .iter()
                    .flat_map(|server| &server.networks)
                    .flat_map(|network| &network.members)
                    .filter_map(|member| member.avatar.clone()),
            )
            .filter(|id| !self.pictures.borrow().contains_key(id))
            .collect();
        for id in ids {
            if !self.fetching.borrow_mut().insert(id.clone()) {
                continue;
            }
            let app = self.clone();
            self.spawn(async move {
                let image = match app.send(Request::Avatar { id: id.clone() }, None).await {
                    Ok(Response::Avatar(Some(image))) => weft_ipc::decode_image(&image),
                    _ => None,
                };
                let pixels = match image {
                    Some(image) => daemon::background(async move { picture::pixels(&image) }).await,
                    None => None,
                };
                let Some(pixels) = pixels else {
                    // The daemon may still be fetching it from the server: ask again later.
                    daemon::background(tokio::time::sleep(Duration::from_secs(5))).await;
                    app.fetching.borrow_mut().remove(&id);
                    return;
                };
                app.fetching.borrow_mut().remove(&id);
                app.pictures.borrow_mut().insert(id, slint::Image::from_rgba8(pixels));
                app.render();
                app.show_own_picture();
            });
        }
    }

    fn power(self: &Rc<Self>) {
        let request = if self.ui().get_connection() == "disconnected" {
            Request::Up { link: None, nickname: None }
        } else {
            Request::Down
        };
        self.request_then_poll(request);
    }

    fn request_then_poll(self: &Rc<Self>, request: Request) {
        let app = self.clone();
        self.spawn(async move {
            if let Err(error) = app.send(request, None).await {
                app.toast(&error, true);
            }
            app.poll();
        });
    }

    fn toggle_theme(&self) {
        let ui = self.ui();
        let theme = ui.global::<Theme>();
        let dark = !theme.get_dark();
        theme.set_dark(dark);
        self.store.borrow_mut().update(|settings| settings.dark = Some(dark));
    }

    fn toggle_network(&self, index: usize) {
        let networks = self.networks.borrow();
        let Some(mut card) = networks.cards.row_data(index) else { return };
        card.collapsed = !card.collapsed;
        let id = card.id.to_string();
        let collapsed = card.collapsed;
        networks.cards.set_row_data(index, card);
        self.store.borrow_mut().update(|settings| {
            settings.collapsed.retain(|known| *known != id);
            if collapsed {
                settings.collapsed.push(id);
            }
        });
    }

    fn rename(self: &Rc<Self>, name: String, close: bool) {
        let name = name.trim().to_string();
        let current = self.status.borrow().as_ref().map(|status| status.nickname.clone());
        let app = self.clone();
        self.spawn(async move {
            if !name.is_empty()
                && Some(&name) != current.as_ref()
                && let Err(error) = app.send(Request::Up { link: None, nickname: Some(name) }, None).await
            {
                if close {
                    app.fail(&error);
                    return;
                }
                app.toast(&error, true);
            }
            if close {
                app.close();
            }
            app.poll();
        });
    }

    fn check_updates(self: &Rc<Self>) {
        let ui = self.ui();
        if !self.store.borrow().current.updates {
            ui.set_update_version(SharedString::new());
            return;
        }
        // The last known release shows right away; GitHub is asked again on every start.
        if let Some(release) = self.store.borrow().current.release.clone() {
            self.show_release(&release.tag);
        }
        let app = self.clone();
        self.spawn(async move {
            let fetched =
                daemon::background(async { tokio::task::spawn_blocking(update::fetch).await.ok().flatten() }).await;
            if let Some(release) = fetched {
                app.show_release(&release.tag);
                app.store.borrow_mut().update(|settings| settings.release = Some(release));
            }
        });
    }

    fn show_release(&self, tag: &str) {
        let version = if update::newer(tag) { tag.trim_start_matches('v') } else { "" };
        self.ui().set_update_version(version.into());
    }

    /// Downloads the new version and starts its installer; without a package for this system
    /// it opens the release page instead.
    fn download_update(self: &Rc<Self>) {
        let ui = self.ui();
        if ui.get_update_stage() != "" {
            return;
        }
        ui.set_update_stage("downloading".into());
        ui.set_update_progress(0);
        let app = self.clone();
        self.spawn(async move {
            let cached = app.store.borrow().current.release.clone();
            let release = match cached.filter(|release| !release.assets.is_empty()) {
                Some(release) => Some(release),
                None => daemon::background(async { tokio::task::spawn_blocking(update::fetch).await.ok().flatten() })
                    .await
                    .inspect(|release| {
                        let release = release.clone();
                        app.store.borrow_mut().update(|settings| settings.release = Some(release));
                    }),
            };
            let Some(release) = release else {
                app.ui().set_update_stage(SharedString::new());
                return app.toast(&app.tr("gui-update-failed"), true);
            };
            let Some(plan) = update::plan(&release) else {
                app.ui().set_update_stage(SharedString::new());
                if release.url.starts_with(update::RELEASES)
                    && let Err(error) = system::open_url(&release.url)
                {
                    app.toast(&error, true);
                }
                return;
            };
            let weak = app.ui.clone();
            let asset = plan.asset.clone();
            let downloaded = daemon::background(async move {
                tokio::task::spawn_blocking(move || {
                    update::download(&asset, |percent| {
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_update_progress(i32::from(percent));
                            }
                        });
                    })
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()))
            })
            .await;
            let path = match downloaded {
                Ok(path) => path,
                Err(reason) => {
                    app.ui().set_update_stage(SharedString::new());
                    return app.toast(&app.tr_args("gui-update-error", &[("reason", &reason)]), true);
                }
            };
            app.ui().set_update_stage("installing".into());
            let installing = plan.clone();
            let installed = daemon::background(async move {
                tokio::task::spawn_blocking(move || update::install(&installing, &path))
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            })
            .await;
            match installed {
                // The installer replaces this program and starts the new version itself.
                Ok(()) if plan.installer == update::Installer::Msi => {
                    let _ = slint::quit_event_loop();
                }
                Ok(()) => {
                    app.ui().set_update_stage(SharedString::new());
                    if plan.installer != update::Installer::Pkg {
                        app.toast(&app.tr("gui-update-done"), false);
                    }
                }
                Err(reason) => {
                    app.ui().set_update_stage(SharedString::new());
                    app.toast(&app.tr_args("gui-update-error", &[("reason", &reason)]), true);
                }
            }
        });
    }

    // Dialogs

    fn open(&self, dialog: &str) {
        let ui = self.ui();
        ui.set_dialog_error(SharedString::new());
        ui.set_dialog_busy(false);
        ui.set_dialog(dialog.into());
    }

    fn close(&self) {
        self.ui().set_dialog(SharedString::new());
    }

    fn fail(&self, error: &str) {
        let ui = self.ui();
        ui.set_dialog_error(error.into());
        ui.set_dialog_busy(false);
    }

    /// Goes back from a submenu, or closes the dialog.
    fn leave(self: &Rc<Self>) {
        match self.ui().get_dialog().as_str() {
            "servers" | "notifications" => {
                let ui = self.ui();
                let nickname = ui.get_settings_nickname();
                self.open_settings();
                ui.set_settings_nickname(nickname);
            }
            "server-menu" | "host" | "add-server" => self.open_servers(),
            _ => self.close(),
        }
    }

    /// Runs a dialog's action with the dialog marked busy; an error stays in the dialog.
    fn run(&self, task: impl Future<Output = Result<(), String>> + 'static) {
        let ui = self.ui();
        ui.set_dialog_error(SharedString::new());
        ui.set_dialog_busy(true);
        let weak = self.ui.clone();
        self.spawn(async move {
            let result = task.await;
            if let Some(ui) = weak.upgrade() {
                ui.set_dialog_busy(false);
                if let Err(error) = result {
                    ui.set_dialog_error(error.into());
                }
            }
        });
    }

    /// Opens a dialog by name, for screenshots of the demo state.
    fn open_by_name(self: &Rc<Self>, name: &str) {
        match name {
            "create" => self.open_create(),
            "join" => self.open_join(),
            "settings" => self.open_settings(),
            "servers" => self.open_servers(),
            "add-server" => self.open_add_server(),
            "invites" => self.with_network(0, App::open_invites),
            "network-menu" => self.with_network(0, App::open_network_menu),
            "network-settings" => self.with_network(0, App::open_network_settings),
            "member" => self.open_member(0, 2),
            "member-admin" => self.open_member(0, 1),
            "member-muted" => self.open_member(0, 3),
            "notifications" => self.open_notifications(),
            "toast" => self.toast(&self.tr("gui-copied"), false),
            "off" => {
                let mut status = view::demo();
                for server in &mut status.servers {
                    server.connection = Connection::Disconnected;
                    server.networks.clear();
                }
                self.show_status(status);
            }
            "update" => {
                let ui = self.ui();
                ui.set_update_version("1.1.0".into());
                ui.set_update_stage("downloading".into());
                ui.set_update_progress(42);
            }
            "diagnostics" => self.open_diagnostics(),
            "ssh" => {
                self.open_create();
                let ui = self.ui();
                let chooser = ui.global::<Chooser>();
                chooser.set_index(chooser.get_labels().row_count() as i32 - 1);
                let steps: Vec<Step> = DEPLOY_STEPS
                    .iter()
                    .map(|step| Step {
                        label: self.tr(&format!("gui-deploy-{step}")).into(),
                        state: SharedString::new(),
                    })
                    .collect();
                chooser.set_steps(ModelRc::new(VecModel::from(steps)));
                chooser.set_show_steps(true);
                mark_active(&ui, "configure");
            }
            _ => {}
        }
    }

    // Choosing a server for a new network

    fn prepare_chooser(&self, only_new: bool) {
        let ui = self.ui();
        let chooser = ui.global::<Chooser>();
        let Some(status) = self.status.borrow().clone() else { return };
        let mut choices: Vec<(String, String, String, bool)> = Vec::new();
        if !only_new {
            for server in &status.servers {
                choices.push((server.server.clone(), view::server_name(&self.l, server), String::new(), false));
            }
        }
        if !status.servers.iter().any(|server| server.public) && status.public_link.is_some() {
            choices.push(("new:online".into(), self.tr("gui-mode-online"), self.tr("gui-mode-online-hint"), false));
        }
        if status.host.is_none() {
            choices.push(("new:local".into(), self.tr("gui-mode-local"), self.tr("gui-mode-local-hint"), false));
        }
        choices.push(("new:vps".into(), self.tr("gui-mode-vps"), self.tr("gui-mode-vps-hint"), true));
        let preferred = if only_new {
            0
        } else {
            status.servers.iter().position(|server| server.connection == Connection::Connected).unwrap_or(0)
        };
        let texts = |pick: fn(&(String, String, String, bool)) -> &str| {
            let texts: Vec<SharedString> = choices.iter().map(|choice| pick(choice).into()).collect();
            ModelRc::new(VecModel::from(texts))
        };
        chooser.set_labels(texts(|choice| &choice.1));
        chooser.set_hints(texts(|choice| &choice.2));
        chooser.set_needs_ssh(ModelRc::new(VecModel::from(choices.iter().map(|choice| choice.3).collect::<Vec<_>>())));
        chooser.set_index(preferred as i32);
        chooser.set_host(SharedString::new());
        chooser.set_password(SharedString::new());
        chooser.set_user("root".into());
        chooser.set_port("22".into());
        chooser.set_show_steps(false);
        self.context.borrow_mut().choices = choices.into_iter().map(|choice| choice.0).collect();
    }

    /// Sets the chosen server up when it is new and returns the link to address it by.
    async fn resolve(self: &Rc<Self>) -> Result<String, String> {
        let index = self.ui().global::<Chooser>().get_index() as usize;
        let choice = self.context.borrow().choices.get(index).cloned().unwrap_or_default();
        let before: HashSet<String> = self
            .status
            .borrow()
            .iter()
            .flat_map(|status| status.servers.iter().map(|server| server.server.clone()))
            .collect();
        match choice.as_str() {
            "new:online" => {
                let link = self.status.borrow().as_ref().and_then(|status| status.public_link.clone());
                self.send(Request::Up { link, nickname: None }, None).await?;
                self.connected(|server| server.public).await
            }
            "new:local" => {
                self.send(Request::Host { enabled: true, port: None, address: None }, None).await?;
                self.connected(|server| server.hosted).await
            }
            "new:vps" => {
                let deployed = self.deploy().await?;
                let link = deployed.link.clone();
                self.send(Request::Up { link: Some(link.clone()), nickname: None }, None).await?;
                let server = self
                    .connected(move |server| {
                        server.server == link || (!server.public && !server.hosted && !before.contains(&server.server))
                    })
                    .await?;
                if let Some(panel) = deployed.panel {
                    let key = server.clone();
                    self.store.borrow_mut().update(|settings| {
                        settings.panels.insert(key, panel);
                    });
                }
                Ok(server)
            }
            _ => Ok(choice),
        }
    }

    /// Waits until the server `pick` finds is connected and returns its link.
    async fn connected(self: &Rc<Self>, pick: impl Fn(&ServerStatus) -> bool) -> Result<String, String> {
        let until = Instant::now() + CONNECT_WAIT;
        loop {
            if let Some(status) = self.refresh().await
                && let Some(server) = status.servers.iter().find(|server| pick(server))
                && server.connection == Connection::Connected
            {
                return Ok(server.server.clone());
            }
            if Instant::now() > until {
                return Err(self.tr("gui-server-unreachable"));
            }
            daemon::background(tokio::time::sleep(Duration::from_millis(700))).await;
        }
    }

    /// Installs the server over SSH, showing its steps, and returns its link.
    async fn deploy(self: &Rc<Self>) -> Result<deploy::Deployed, String> {
        let ui = self.ui();
        let chooser = ui.global::<Chooser>();
        let host = chooser.get_host().trim().to_string();
        let password = chooser.get_password().to_string();
        if host.is_empty() || password.is_empty() {
            return Err(self.tr("gui-vps-missing"));
        }
        let user = chooser.get_user().trim().to_string();
        let target = deploy::Target {
            host,
            port: chooser.get_port().trim().parse().unwrap_or(22),
            user: if user.is_empty() { "root".into() } else { user },
            password,
            binary: None,
        };
        let steps: Vec<Step> = DEPLOY_STEPS
            .iter()
            .map(|step| Step { label: self.tr(&format!("gui-deploy-{step}")).into(), state: SharedString::new() })
            .collect();
        chooser.set_steps(ModelRc::new(VecModel::from(steps)));
        chooser.set_show_steps(true);

        let weak = self.ui.clone();
        let result = daemon::background(async move {
            deploy::deploy(&target, move |step| {
                let (weak, step) = (weak.clone(), step.to_string());
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        mark_active(&ui, &step);
                    }
                });
            })
            .await
        })
        .await;
        match result {
            Ok(deployed) => {
                mark_all(&ui, |_| "done".into());
                Ok(deployed)
            }
            Err(error) => {
                mark_all(&ui, |state| if state == "active" { "failed".into() } else { state.into() });
                Err(match error {
                    deploy::DeployError::Unreachable(reason) => {
                        self.tr_args("gui-ssh-unreachable", &[("reason", &reason)])
                    }
                    deploy::DeployError::Login => self.tr("gui-ssh-login-failed"),
                    deploy::DeployError::HostKeyChanged => self.tr("gui-ssh-host-changed"),
                    deploy::DeployError::Script(code) => self.tr(&format!("gui-ssh-error-{code}")),
                    deploy::DeployError::Other(reason) => self.tr_args("gui-ssh-failed", &[("reason", &reason)]),
                })
            }
        }
    }

    fn open_create(&self) {
        self.prepare_chooser(false);
        self.open("create");
    }

    fn open_join(&self) {
        self.prepare_chooser(false);
        self.open("join");
    }

    fn create_network(self: &Rc<Self>, name: String, password: String, repeat: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return self.fail(&self.tr("gui-name-missing"));
        }
        if password != repeat {
            return self.fail(&self.tr("password-mismatch"));
        }
        let app = self.clone();
        self.run(async move {
            let server = app.resolve().await?;
            app.send(Request::Create { name: name.clone(), password }, Some(server)).await?;
            app.toast(&app.tr_args("done-create", &[("name", &name)]), false);
            app.close();
            app.poll();
            Ok(())
        });
    }

    fn join_network(self: &Rc<Self>, target: String, password: String) {
        let target = target.trim().to_string();
        let app = self.clone();
        self.run(async move {
            let response = if target.to_lowercase().starts_with("weft://") {
                app.send(Request::Redeem { link: target.clone() }, None).await?
            } else {
                let server = app.resolve().await?;
                app.send(Request::Join { name: target.clone(), password }, Some(server)).await?
            };
            let text = match &response {
                Response::Pending(name) => app.tr_args("done-pending", &[("name", name)]),
                Response::Joined(name) => app.tr_args("done-join", &[("name", name)]),
                _ => app.tr_args("done-join", &[("name", &target)]),
            };
            app.toast(&text, false);
            app.close();
            app.poll();
            Ok(())
        });
    }

    // Settings and servers

    fn open_settings(&self) {
        let ui = self.ui();
        let status = self.status.borrow().clone();
        ui.set_settings_nickname(status.as_ref().map(|status| status.nickname.clone()).unwrap_or_default().into());
        let languages: Vec<SharedString> = Language::ALL.iter().map(|language| language.name().into()).collect();
        ui.set_languages(ModelRc::new(VecModel::from(languages)));
        let index = Language::ALL.iter().position(|language| *language == self.l.language()).unwrap_or(0);
        ui.set_language_index(index as i32);
        ui.set_server_count(status.as_ref().map_or(0, |status| view::servers(status).len()) as i32);
        let notifications = self.store.borrow().current.notifications;
        ui.set_notifications_detail(self.tr(if notifications { "gui-on" } else { "gui-off" }).into());
        ui.set_updates(self.store.borrow().current.updates);
        self.show_own_picture();
        self.open("settings");
    }

    /// The own picture, or the first letter, in the settings.
    fn show_own_picture(&self) {
        let ui = self.ui();
        let status = self.status.borrow();
        let nickname = status.as_ref().map(|status| status.nickname.as_str()).unwrap_or_default();
        ui.set_own_initial(view::initial(nickname).into());
        ui.set_own_tone(view::avatar_tone(nickname));
        let picture = status
            .as_ref()
            .and_then(|status| status.avatar.as_ref())
            .and_then(|id| self.pictures.borrow().get(id).cloned());
        ui.set_own_has_picture(picture.is_some());
        ui.set_own_picture(picture.unwrap_or_default());
    }

    fn open_notifications(&self) {
        let ui = self.ui();
        let current = self.store.borrow().current.clone();
        ui.set_notifications(current.notifications);
        let kinds: Vec<SharedString> = settings::Notify::KINDS.iter().map(|&kind| kind.into()).collect();
        let labels: Vec<SharedString> =
            settings::Notify::KINDS.iter().map(|kind| self.tr(&format!("gui-notify-{kind}")).into()).collect();
        let wanted: Vec<bool> = settings::Notify::KINDS.iter().map(|kind| current.notify.get(kind)).collect();
        ui.set_notify_kinds(ModelRc::new(VecModel::from(kinds)));
        ui.set_notify_labels(ModelRc::new(VecModel::from(labels)));
        ui.set_notify_wanted(ModelRc::new(VecModel::from(wanted)));
        self.open("notifications");
    }

    fn toggle_notify(&self, kind: &str, on: bool) {
        self.store.borrow_mut().update(|settings| settings.notify.set(kind, on));
    }

    fn pick_picture(self: &Rc<Self>) {
        let title = self.tr("gui-picture-choose");
        let filter = self.tr("gui-picture-files");
        let app = self.clone();
        self.spawn(async move {
            let dialog = rfd::AsyncFileDialog::new()
                .set_title(&title)
                .add_filter(&filter, &["png", "jpg", "jpeg", "webp", "gif", "bmp"]);
            let Some(file) = dialog.pick_file().await else { return };
            let path = file.path().to_path_buf();
            let prepared = daemon::background(async move { picture::prepare(&path) }).await;
            match prepared {
                Ok(image) => app.set_picture(Some(image)),
                Err(picture::PictureError::TooBig) => app.toast(&app.tr("gui-picture-too-big"), true),
                Err(picture::PictureError::Unreadable) => app.toast(&app.tr("gui-picture-unreadable"), true),
            }
        });
    }

    fn set_picture(self: &Rc<Self>, image: Option<Vec<u8>>) {
        let removed = image.is_none();
        let app = self.clone();
        self.run(async move {
            let image = image.as_deref().map(weft_ipc::encode_image);
            app.send(Request::SetAvatar { image }, None).await?;
            app.refresh().await;
            app.show_own_picture();
            app.toast(&app.tr(if removed { "done-picture-removed" } else { "done-picture" }), false);
            Ok(())
        });
    }

    fn choose_language(&self, index: usize) {
        let language = Language::ALL.get(index).copied().unwrap_or(Language::English);
        self.store.borrow_mut().update(|settings| settings.language = Some(language.code().into()));
        self.l.set_language(language);
        let ui = self.ui();
        let i18n = ui.global::<I18n>();
        i18n.set_rtl(self.l.language().is_rtl());
        i18n.set_revision(i18n.get_revision() + 1);
        self.tray_texts();
        self.render();
        let nickname = ui.get_settings_nickname();
        self.open_settings();
        ui.set_settings_nickname(nickname);
    }

    /// Forgets every server and the app's preferences, keeping the language, and starts over.
    fn reset(self: &Rc<Self>) {
        let app = self.clone();
        self.run(async move {
            let status = app.refresh().await.ok_or_else(|| app.tr("error-not-connected"))?;
            if status.host.is_some() {
                app.send(Request::Host { enabled: false, port: None, address: None }, None).await?;
            }
            for server in status.servers.iter().filter(|server| !server.hosted) {
                app.send(Request::Remove, Some(server.server.clone())).await?;
            }
            app.store.borrow_mut().update(|settings| {
                *settings = settings::Settings { language: settings.language.clone(), ..Default::default() };
            });
            app.ui().global::<Theme>().set_dark(false);
            app.close();
            app.refresh().await;
            app.toast(&app.tr("done-reset"), false);
            Ok(())
        });
    }

    fn open_servers(&self) {
        if let Some(status) = self.status.borrow().as_ref() {
            view::sync(&self.servers, view::server_rows(&self.l, status));
        }
        self.open("servers");
    }

    fn open_server_menu(&self, index: usize) {
        let Some(status) = self.status.borrow().clone() else { return };
        let Some(server) = view::servers(&status).get(index).cloned() else { return };
        let ui = self.ui();
        ui.set_dialog_title(view::server_name(&self.l, &server).into());
        ui.set_menu_local(server.hosted);
        ui.set_menu_has_link(!server.hosted || status.host.as_ref().is_some_and(|host| host.link.is_some()));
        ui.set_menu_has_panel(self.panel_for(&server).is_some());
        self.context.borrow_mut().server = Some(server);
        self.open("server-menu");
    }

    /// The web panel of a server: the daemon knows it for the hosted one, the app remembers the
    /// ones it installed over SSH.
    fn panel_for(&self, server: &ServerStatus) -> Option<weft_ipc::PanelAccess> {
        if server.hosted {
            return self.status.borrow().as_ref()?.host.as_ref()?.panel.clone();
        }
        self.store.borrow().current.panels.get(&server.server).cloned()
    }

    fn open_panel(&self) {
        let Some(panel) = self.context.borrow().server.as_ref().and_then(|server| self.panel_for(server)) else {
            return;
        };
        // The address came from the server; only a web page may be opened.
        if !panel.url.starts_with("https://") {
            return;
        }
        if let Err(error) = system::open_url(&panel.url) {
            return self.toast(&error, true);
        }
        if system::copy(&panel.password).is_ok() {
            self.toast(&self.tr("done-panel-password"), false);
        }
    }

    fn copy_panel_password(&self) {
        let Some(panel) = self.context.borrow().server.as_ref().and_then(|server| self.panel_for(server)) else {
            return;
        };
        match system::copy(&panel.password) {
            Ok(()) => self.toast(&self.tr("done-panel-password"), false),
            Err(error) => self.toast(&error, true),
        }
    }

    fn copy_server_link(&self) {
        let Some(server) = self.context.borrow().server.clone() else { return };
        let link = if server.hosted {
            self.status.borrow().as_ref().and_then(|status| status.host.as_ref()).and_then(|host| host.link.clone())
        } else {
            Some(server.server)
        };
        if let Some(link) = link {
            self.copy(&link);
        }
        self.open_servers();
    }

    fn remove_server(self: &Rc<Self>) {
        let Some(server) = self.context.borrow().server.clone() else { return };
        let app = self.clone();
        self.run(async move {
            if server.hosted {
                app.send(Request::Host { enabled: false, port: None, address: None }, None).await?;
            } else {
                app.send(Request::Remove, Some(server.server.clone())).await?;
            }
            let name = view::server_name(&app.l, &server);
            app.toast(&app.tr_args("done-remove", &[("server", &name)]), false);
            app.refresh().await;
            app.open_servers();
            Ok(())
        });
    }

    fn open_host(&self) {
        let Some(host) = self.status.borrow().as_ref().and_then(|status| status.host.clone()) else { return };
        let ui = self.ui();
        ui.set_host_address(host.address.unwrap_or_default().into());
        ui.set_host_port(host.port.to_string().into());
        self.open("host");
    }

    fn save_host(self: &Rc<Self>, address: String, port: String) {
        let port = port.trim().parse::<u16>().ok().filter(|port| *port > 0);
        let app = self.clone();
        self.run(async move {
            let request = Request::Host { enabled: true, port, address: Some(address.trim().to_string()) };
            app.send(request, None).await?;
            app.refresh().await;
            app.open_servers();
            Ok(())
        });
    }

    fn open_add_server(&self) {
        self.prepare_chooser(true);
        self.open("add-server");
    }

    fn add_server(self: &Rc<Self>) {
        let app = self.clone();
        self.run(async move {
            app.resolve().await?;
            app.toast(&app.tr("gui-server-added"), false);
            app.open_servers();
            Ok(())
        });
    }

    // Networks

    fn with_network(self: &Rc<Self>, index: usize, then: impl FnOnce(&Rc<Self>)) {
        let shown = self.networks.borrow().shown.get(index).cloned();
        if let Some(shown) = shown {
            self.context.borrow_mut().network = Some(shown);
            then(self);
        }
    }

    fn network(&self) -> Option<(String, String, Role)> {
        let context = self.context.borrow();
        let (server, network) = context.network.as_ref()?;
        Some((network.name.clone(), server.server.clone(), network.role))
    }

    fn open_network_menu(self: &Rc<Self>) {
        let Some((name, _, role)) = self.network() else { return };
        let ui = self.ui();
        ui.set_dialog_title(name.into());
        ui.set_network_manager(role != Role::Member);
        self.open("network-menu");
    }

    fn network_pick(self: &Rc<Self>, item: &str) {
        match item {
            "invites" => self.open_invites(),
            "requests" | "bans" => self.open_devices(item),
            "settings" => self.open_network_settings(),
            "leave" => {
                let Some((name, server, _)) = self.network() else { return };
                let app = self.clone();
                self.run(async move {
                    app.send(Request::Leave { name: name.clone() }, Some(server)).await?;
                    app.toast(&app.tr_args("done-leave", &[("name", &name)]), false);
                    app.close();
                    app.poll();
                    Ok(())
                });
            }
            _ => {}
        }
    }

    fn open_invites(self: &Rc<Self>) {
        let Some((name, _, _)) = self.network() else { return };
        let ui = self.ui();
        ui.set_list_network(name.into());
        ui.set_invite_created(SharedString::new());
        let expiries: Vec<SharedString> = EXPIRY.iter().map(|(id, _)| SharedString::from(self.tr(id))).collect();
        ui.set_expiries(ModelRc::new(VecModel::from(expiries)));
        self.invites.clear();
        self.open("invites");
        self.load_invites();
    }

    fn load_invites(self: &Rc<Self>) {
        let Some((name, server, _)) = self.network() else { return };
        if self.demo {
            let link = "weft://141.11.211.11:8443/K7QF-M2XA#k=g4xfbqwhwx3slxpw73sszriu2w5bsgms7ahkso2mxuk7253bbn7a";
            self.ui().set_invite_created(link.into());
            let row = InviteRow { code: "K7QF-M2XA".into(), link: link.into(), details: "0/5 \u{b7} 6 d 23 h".into() };
            view::sync(&self.invites, vec![row]);
            return;
        }
        let app = self.clone();
        self.spawn(async move {
            match app.send(Request::Invites { network: name }, Some(server)).await {
                Ok(Response::Invites(invites)) => {
                    let rows = invites
                        .iter()
                        .map(|invite| InviteRow {
                            code: invite.code.clone().into(),
                            link: invite.link.clone().unwrap_or_default().into(),
                            details: app.invite_details(invite).into(),
                        })
                        .collect();
                    view::sync(&app.invites, rows);
                }
                Ok(_) => {}
                Err(error) => app.fail(&error),
            }
        });
    }

    fn invite_details(&self, invite: &weft_ipc::InviteInfo) -> String {
        let uses = invite.uses.to_string();
        let used = match invite.max_uses {
            Some(max) => self.tr_args("invite-uses", &[("uses", &uses), ("max", &max.to_string())]),
            None => self.tr_args("invite-uses-unlimited", &[("uses", &uses)]),
        };
        let expires = match invite.expires {
            Some(at) => {
                let left = view::remaining(&self.l, at.saturating_sub(update::now()));
                self.tr_args("invite-expires", &[("time", &left)])
            }
            None => self.tr("invite-no-expiry"),
        };
        let by = self.tr_args("invite-by", &[("nickname", &invite.creator)]);
        format!("{used} \u{b7} {expires} \u{b7} {by}")
    }

    fn create_invite(self: &Rc<Self>, uses: String, expiry: usize) {
        let Some((network, server, _)) = self.network() else { return };
        let uses = uses.trim();
        let uses = if uses.is_empty() {
            None
        } else {
            match uses.parse::<u32>() {
                Ok(uses) if uses > 0 => Some(uses),
                _ => return self.fail(&self.tr("error-invalid-uses")),
            }
        };
        let expires_in = EXPIRY.get(expiry).and_then(|(_, seconds)| *seconds);
        let app = self.clone();
        self.run(async move {
            if let Response::Invite(invite) =
                app.send(Request::CreateInvite { network, uses, expires_in }, Some(server)).await?
            {
                let link = invite.link.unwrap_or(invite.code);
                app.ui().set_invite_created(link.clone().into());
                if system::copy(&link).is_ok() {
                    app.toast(&app.tr("done-invite-copied"), false);
                }
            }
            app.load_invites();
            Ok(())
        });
    }

    fn revoke_invite(self: &Rc<Self>, code: String) {
        let Some((_, server, _)) = self.network() else { return };
        let app = self.clone();
        self.run(async move {
            app.send(Request::RevokeInvite { code }, Some(server)).await?;
            app.load_invites();
            Ok(())
        });
    }

    fn open_devices(self: &Rc<Self>, kind: &str) {
        let Some((name, _, _)) = self.network() else { return };
        let ui = self.ui();
        ui.set_devices_kind(kind.into());
        ui.set_list_network(name.into());
        self.devices.clear();
        self.open("devices");
        self.load_devices();
    }

    fn load_devices(self: &Rc<Self>) {
        let Some((network, server, _)) = self.network() else { return };
        if self.demo {
            return;
        }
        let requests = self.ui().get_devices_kind() == "requests";
        let app = self.clone();
        self.spawn(async move {
            let request = if requests { Request::Requests { network } } else { Request::Bans { network } };
            match app.send(request, Some(server)).await {
                Ok(Response::Requests(devices) | Response::Bans(devices)) => {
                    let rows = devices
                        .iter()
                        .map(|device| DeviceRow {
                            nickname: device.nickname.clone().into(),
                            address: device.address.to_string().into(),
                            key: device.public_key.clone().into(),
                        })
                        .collect();
                    view::sync(&app.devices, rows);
                }
                Ok(_) => {}
                Err(error) => app.fail(&error),
            }
        });
    }

    fn device_action(self: &Rc<Self>, command: String, key: String) {
        let Some((network, server, _)) = self.network() else { return };
        let app = self.clone();
        self.run(async move {
            let request = match command.as_str() {
                "approve" => Request::Approve { network, member: key },
                "deny" => Request::Deny { network, member: key },
                _ => Request::Unban { network, member: key },
            };
            app.send(request, Some(server)).await?;
            app.poll();
            app.load_devices();
            Ok(())
        });
    }

    fn open_network_settings(self: &Rc<Self>) {
        let Some((_, network)) = self.context.borrow().network.clone() else { return };
        let ui = self.ui();
        ui.set_network_owner(network.role == Role::Owner);
        ui.set_network_locked(network.locked);
        ui.set_network_approval(network.approval);
        ui.set_network_name(network.name.clone().into());
        self.open("network-settings");
    }

    fn rename_network(self: &Rc<Self>, name: String) {
        let Some((network, server, _)) = self.network() else { return };
        let name = name.trim().to_string();
        if name.is_empty() || name == network {
            return;
        }
        let app = self.clone();
        self.run(async move {
            app.send(Request::RenameNetwork { network: network.clone(), name: name.clone() }, Some(server)).await?;
            let (old, new) = (view::network_id_of(&network), view::network_id_of(&name));
            app.store.borrow_mut().update(|settings| {
                for id in &mut settings.collapsed {
                    if let Some(server) = id.strip_suffix(&old) {
                        *id = format!("{server}{new}");
                    }
                }
            });
            app.toast(&app.tr_args("done-rename", &[("name", &network), ("new", &name)]), false);
            app.close();
            app.poll();
            Ok(())
        });
    }

    fn configure(self: &Rc<Self>, key: String, on: bool) {
        let Some((network, server, _)) = self.network() else { return };
        let app = self.clone();
        self.spawn(async move {
            let request = Request::Configure {
                network,
                locked: (key == "locked").then_some(on),
                approval: (key == "approval").then_some(on),
                password: None,
            };
            match app.send(request, Some(server)).await {
                Ok(_) => app.poll(),
                Err(error) => {
                    let ui = app.ui();
                    if key == "locked" {
                        ui.set_network_locked(!on);
                    } else {
                        ui.set_network_approval(!on);
                    }
                    app.fail(&error);
                }
            }
        });
    }

    fn change_password(self: &Rc<Self>, password: String) {
        let Some((network, server, _)) = self.network() else { return };
        let app = self.clone();
        self.run(async move {
            let request =
                Request::Configure { network: network.clone(), locked: None, approval: None, password: Some(password) };
            app.send(request, Some(server)).await?;
            app.toast(&app.tr_args("done-password", &[("name", &network)]), false);
            Ok(())
        });
    }

    fn delete_network(self: &Rc<Self>) {
        let Some((network, server, _)) = self.network() else { return };
        let app = self.clone();
        self.run(async move {
            app.send(Request::Delete { network: network.clone() }, Some(server)).await?;
            app.toast(&app.tr_args("done-delete", &[("name", &network)]), false);
            app.close();
            app.poll();
            Ok(())
        });
    }

    fn open_member(&self, network: usize, member: usize) {
        let shown = self.networks.borrow().shown.get(network).cloned();
        let Some((server, network)) = shown else { return };
        let Some(row) = member.checked_sub(1).and_then(|index| network.members.get(index)).cloned() else { return };
        let ui = self.ui();
        ui.set_dialog_title(row.nickname.clone().into());
        ui.set_member_address(row.address.to_string().into());
        ui.set_network_owner(network.role == Role::Owner);
        let role = row.role.unwrap_or(Role::Member);
        ui.set_member_admin(role == Role::Admin);
        ui.set_member_above(view::rank(network.role) > view::rank(role));
        ui.set_member_muted(row.muted_until.is_some());
        let muted = row.muted_until.map(|until| view::muted_text(&self.l, "gui-muted", until));
        ui.set_member_muted_text(muted.unwrap_or_default().into());
        let mut durations: Vec<SharedString> = MUTES.iter().map(|(id, _)| self.tr(id).into()).collect();
        durations.push(self.tr("gui-mute-custom").into());
        durations.push(self.tr("gui-mute-forever").into());
        ui.set_mute_durations(ModelRc::new(VecModel::from(durations)));
        let units: Vec<SharedString> = MUTE_UNITS.iter().map(|(id, _)| self.tr(id).into()).collect();
        ui.set_mute_units(ModelRc::new(VecModel::from(units)));
        let mut context = self.context.borrow_mut();
        context.member = Some((row.address.to_string(), row.nickname, row.muted_until.is_some()));
        context.network = Some((server, network));
        drop(context);
        self.open("member");
    }

    /// Mutes the member for a listed time, a custom one, or for good.
    fn mute_member(self: &Rc<Self>, duration: usize, amount: String, unit: usize) {
        let seconds = match duration {
            index if index < MUTES.len() => Some(MUTES[index].1),
            index if index == MUTES.len() => {
                let amount = amount.trim().parse::<u64>().ok().filter(|&amount| amount > 0);
                let unit = MUTE_UNITS.get(unit).map_or(60, |(_, seconds)| *seconds);
                match amount.and_then(|amount| amount.checked_mul(unit)) {
                    Some(seconds) => Some(seconds),
                    None => return self.fail(&self.tr("gui-mute-invalid")),
                }
            }
            _ => None,
        };
        let Some((network, server, _)) = self.network() else { return };
        let Some((member, nickname, _)) = self.context.borrow().member.clone() else { return };
        let app = self.clone();
        self.run(async move {
            let name = network.clone();
            app.send(Request::Mute { network, member, seconds }, Some(server)).await?;
            let args = [("name", name.as_str()), ("member", nickname.as_str())];
            let done = match seconds {
                Some(seconds) => {
                    let time = view::remaining(&app.l, seconds);
                    app.tr_args("done-mute", &[args[0], args[1], ("time", &time)])
                }
                None => app.tr_args("done-mute-forever", &args),
            };
            app.toast(&done, false);
            app.close();
            app.poll();
            Ok(())
        });
    }

    fn member_action(self: &Rc<Self>, command: String) {
        let Some((network, server, _)) = self.network() else { return };
        let Some((member, nickname, _)) = self.context.borrow().member.clone() else { return };
        let app = self.clone();
        self.run(async move {
            let name = network.clone();
            let (request, done) = match command.as_str() {
                "unmute" => (Request::Mute { network, member, seconds: Some(0) }, "done-unmute"),
                "kick" => (Request::Kick { network, member }, "done-kick"),
                "ban" => (Request::Ban { network, member }, "done-ban"),
                "promote" => (Request::SetRole { network, member, role: Role::Admin }, "done-promote"),
                _ => (Request::SetRole { network, member, role: Role::Member }, "done-demote"),
            };
            app.send(request, Some(server)).await?;
            app.toast(&app.tr_args(done, &[("name", &name), ("member", &nickname)]), false);
            app.close();
            app.poll();
            Ok(())
        });
    }

    // Diagnostics

    fn open_diagnostics(self: &Rc<Self>) {
        self.ui().set_report(SharedString::new());
        self.open("diagnostics");
        self.load_report();
    }

    fn load_report(self: &Rc<Self>) {
        if self.demo {
            return;
        }
        let app = self.clone();
        self.run(async move {
            let report = daemon::report(&app.l, false).await?;
            app.ui().set_report(report.into());
            Ok(())
        });
    }

    fn save_report(self: &Rc<Self>) {
        let app = self.clone();
        self.run(async move {
            let report = daemon::report(&app.l, true).await?;
            let path = system::downloads_dir().join(format!("weft-report-{}.txt", update::now()));
            std::fs::write(&path, report).map_err(|error| error.to_string())?;
            app.toast(&app.tr_args("done-report-saved", &[("path", &path.display().to_string())]), false);
            Ok(())
        });
    }
}

/// Marks the deploy steps before `name` done and `name` itself active.
fn mark_active(ui: &AppWindow, name: &str) {
    let Some(position) = DEPLOY_STEPS.iter().position(|step| *step == name) else { return };
    mark(ui, |index, _| match index.cmp(&position) {
        std::cmp::Ordering::Less => "done".into(),
        std::cmp::Ordering::Equal => "active".into(),
        std::cmp::Ordering::Greater => SharedString::new(),
    });
}

fn mark_all(ui: &AppWindow, state: impl Fn(&str) -> SharedString) {
    mark(ui, |_, current| state(current));
}

fn mark(ui: &AppWindow, state: impl Fn(usize, &str) -> SharedString) {
    let steps = ui.global::<Chooser>().get_steps();
    for index in 0..steps.row_count() {
        if let Some(mut step) = steps.row_data(index) {
            step.state = state(index, &step.state);
            steps.set_row_data(index, step);
        }
    }
}
