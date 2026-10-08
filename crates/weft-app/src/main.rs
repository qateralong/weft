//! Weft desktop app, drawn by Slint without a web engine.

mod shot;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};
use weft_i18n::{Language, Localizer};
use weft_ipc::{Connection, MemberStatus, NetworkStatus, PeerLink, Request, Response, Role, ServerStatus, Status};

mod ui {
    #![allow(clippy::all, clippy::todo)]
    slint::include_modules!();
}

use ui::*;

const POLL: Duration = Duration::from_millis(1500);
const AVATAR_COLORS: [u32; 8] = [0xd9734e, 0xc9a03a, 0x5f9e57, 0x3e9a91, 0x3f7fb8, 0x7867c4, 0xb95f9d, 0x8b7258];

struct Options {
    demo: bool,
    shot: Option<PathBuf>,
    language: Option<Language>,
    dark: bool,
}

fn options() -> Options {
    let mut options = Options { demo: false, shot: None, language: None, dark: false };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--demo" => options.demo = true,
            "--dark" => options.dark = true,
            "--screenshot" => options.shot = args.next().map(PathBuf::from),
            "--lang" => options.language = args.next().as_deref().and_then(Language::from_code),
            _ => {}
        }
    }
    options
}

/// What the window shows, kept between polls so rows are updated in place.
struct View {
    l: Arc<Localizer>,
    networks: Rc<VecModel<NetworkCard>>,
    members: HashMap<String, Rc<VecModel<MemberRow>>>,
    collapsed: HashSet<String>,
    last: Option<Status>,
}

fn main() -> Result<(), slint::PlatformError> {
    let options = options();
    if options.shot.is_some() {
        shot::install(440, 680);
    }
    let l = Arc::new(Localizer::new(options.language.unwrap_or_else(Language::from_env)));
    let ui = AppWindow::new()?;
    let view = Rc::new(RefCell::new(View {
        l: l.clone(),
        networks: Rc::new(VecModel::default()),
        members: HashMap::new(),
        collapsed: HashSet::new(),
        last: None,
    }));
    ui.set_networks(ModelRc::from(view.borrow().networks.clone()));

    let i18n = ui.global::<I18n>();
    let translator = l.clone();
    i18n.on_translate(move |id| translator.tr(&id).into());
    i18n.set_rtl(l.language().is_rtl());
    ui.global::<Theme>().set_dark(options.dark);

    let weak = ui.as_weak();
    ui.on_toggle_theme(move || {
        if let Some(ui) = weak.upgrade() {
            let theme = ui.global::<Theme>();
            theme.set_dark(!theme.get_dark());
        }
    });
    let toggled = view.clone();
    ui.on_toggle_network(move |index| {
        let mut view = toggled.borrow_mut();
        let Some(mut card) = view.networks.row_data(index as usize) else { return };
        card.collapsed = !card.collapsed;
        if card.collapsed {
            view.collapsed.insert(card.id.to_string());
        } else {
            view.collapsed.remove(card.id.as_str());
        }
        view.networks.set_row_data(index as usize, card);
    });
    ui.on_toggle_power(|| spawn_request(Request::Down));

    if options.demo || options.shot.is_some() {
        show(&ui, &view, Ok(demo()));
    } else {
        poll(&ui, view.clone());
    }
    match options.shot {
        Some(path) => shot::save(&ui, &path),
        None => ui.run(),
    }
}

/// Asks the daemon for its state in the background and shows every answer.
fn poll(ui: &AppWindow, view: Rc<RefCell<View>>) {
    let weak = ui.as_weak();
    let (sender, receiver) = std::sync::mpsc::channel::<Result<Status, String>>();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        runtime.block_on(async {
            loop {
                let result = match weft_ipc::request(&weft_ipc::socket_path(), &Request::Status).await {
                    Ok(Response::Status(status)) => Ok(status),
                    Ok(other) => Err(format!("{other:?}")),
                    Err(error) => Err(error.to_string()),
                };
                if sender.send(result).is_err() {
                    return;
                }
                let weak = weak.clone();
                if slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.invoke_poll_tick();
                    }
                })
                .is_err()
                {
                    return;
                }
                tokio::time::sleep(POLL).await;
            }
        });
    });
    let weak = ui.as_weak();
    ui.on_poll_tick(move || {
        if let (Some(ui), Ok(result)) = (weak.upgrade(), receiver.try_recv()) {
            show(&ui, &view, result);
        }
    });
}

fn spawn_request(request: Request) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        let _ = runtime.block_on(weft_ipc::request(&weft_ipc::socket_path(), &request));
    });
}

fn show(ui: &AppWindow, view: &Rc<RefCell<View>>, result: Result<Status, String>) {
    let mut view = view.borrow_mut();
    let status = match result {
        Ok(status) => status,
        Err(error) => {
            ui.set_phase("problem".into());
            ui.set_problem_title(view.l.tr("gui-daemon-problem").into());
            ui.set_problem_text(error.into());
            ui.set_problem_action(view.l.tr("gui-retry").into());
            return;
        }
    };
    let connection = overall(&status.servers);
    ui.set_phase(if status.servers.is_empty() && status.host.is_none() { "welcome" } else { "main" }.into());
    ui.set_nickname(status.nickname.clone().into());
    ui.set_connection(connection.into());
    ui.set_address(
        status.servers.iter().find_map(|server| server.address).map(|a| a.to_string()).unwrap_or_default().into(),
    );

    let several = status.servers.len() > 1;
    let mut cards = Vec::new();
    for server in &status.servers {
        for network in &server.networks {
            let id = format!("{}/{}", server.server, network.name);
            let rows = member_rows(&view.l, &status, server, network, several);
            let members = view.members.entry(id.clone()).or_insert_with(|| Rc::new(VecModel::default())).clone();
            sync(&members, rows);
            cards.push(card(&view.l, &id, server, network, several, view.collapsed.contains(&id), members));
        }
    }
    sync(&view.networks, cards);
    view.last = Some(status);
}

fn overall(servers: &[ServerStatus]) -> &'static str {
    if servers.iter().any(|server| server.connection == Connection::Connected) {
        "connected"
    } else if servers.iter().any(|server| server.connection == Connection::Connecting) {
        "connecting"
    } else {
        "disconnected"
    }
}

fn card(
    l: &Localizer,
    id: &str,
    server: &ServerStatus,
    network: &NetworkStatus,
    several: bool,
    collapsed: bool,
    members: Rc<VecModel<MemberRow>>,
) -> NetworkCard {
    let role = match network.role {
        Role::Owner => "role-owner",
        Role::Admin => "role-admin",
        Role::Member => "role-member",
    };
    let mut tags = vec![SharedString::from(l.tr(role))];
    if network.locked {
        tags.push(l.tr("network-locked").into());
    }
    if network.approval {
        tags.push(l.tr("network-approval").into());
    }
    if several {
        tags.push(server_name(l, server).into());
    }
    if l.language().is_rtl() {
        tags.reverse();
    }
    let online = network.members.iter().filter(|member| member.link != PeerLink::Offline).count();
    NetworkCard {
        id: id.into(),
        name: network.name.clone().into(),
        count: format!("{}/{}", online + 1, network.members.len() + 1).into(),
        tags: ModelRc::new(VecModel::from(tags)),
        members: ModelRc::from(members),
        collapsed,
        requests: network.requests as i32,
        manager: network.role != Role::Member,
    }
}

fn server_name(l: &Localizer, server: &ServerStatus) -> String {
    if server.public {
        l.tr("gui-mode-online")
    } else if server.hosted {
        l.tr("gui-mode-local")
    } else {
        server.host.clone()
    }
}

fn member_rows(
    l: &Localizer,
    status: &Status,
    server: &ServerStatus,
    network: &NetworkStatus,
    several: bool,
) -> Vec<MemberRow> {
    let connected = server.connection == Connection::Connected;
    let to_server = server.latency_ms.filter(|_| connected);
    let state = match server.connection {
        Connection::Connected => "state-connected",
        Connection::Connecting => "state-connecting",
        Connection::Disconnected => "state-disconnected",
    };
    let me = MemberRow {
        nickname: status.nickname.clone().into(),
        initial: initial(&status.nickname).into(),
        color: avatar_color(&status.nickname),
        sub: if several { server_name(l, server) } else { l.tr(state) }.into(),
        address: server.address.map(|a| a.to_string()).unwrap_or_default().into(),
        ping: ping_text(l, to_server, false).into(),
        level: level(to_server),
        link: if connected { "direct" } else { "offline" }.into(),
        me: true,
    };
    let mut rows = vec![me];
    rows.extend(network.members.iter().map(|member| member_row(l, member, to_server)));
    rows
}

fn member_row(l: &Localizer, member: &MemberStatus, to_server: Option<u32>) -> MemberRow {
    let (link, sub, ms, estimate) = match member.link {
        PeerLink::Direct => ("direct", "link-direct", member.latency_ms, false),
        PeerLink::Relay => ("relay", "link-relay", to_server.map(|ms| ms * 2), true),
        PeerLink::Connecting => ("connecting", "link-connecting", None, false),
        PeerLink::Offline => ("offline", "link-offline", None, false),
    };
    MemberRow {
        nickname: member.nickname.clone().into(),
        initial: initial(&member.nickname).into(),
        color: avatar_color(&member.nickname),
        sub: l.tr(sub).into(),
        address: member.address.to_string().into(),
        ping: ping_text(l, ms, estimate).into(),
        level: level(ms),
        link: link.into(),
        me: false,
    }
}

fn ping_text(l: &Localizer, ms: Option<u32>, estimate: bool) -> String {
    match ms {
        None => "\u{2014}".into(),
        Some(0) => l.tr_args("gui-ping-ms", &[("ms", "<1")]),
        Some(ms) => l.tr_args("gui-ping-ms", &[("ms", &format!("{}{ms}", if estimate { "~" } else { "" }))]),
    }
}

fn level(ms: Option<u32>) -> i32 {
    match ms {
        None => 0,
        Some(ms) if ms < 60 => 4,
        Some(ms) if ms < 120 => 3,
        Some(ms) if ms < 250 => 2,
        Some(_) => 1,
    }
}

fn initial(nickname: &str) -> String {
    nickname.chars().next().map_or_else(|| "?".into(), |c| c.to_uppercase().collect())
}

fn avatar_color(nickname: &str) -> Color {
    let hash = nickname.chars().fold(0u32, |hash, c| hash.wrapping_mul(31).wrapping_add(c as u32));
    Color::from_argb_encoded(0xff00_0000 | AVATAR_COLORS[hash as usize % AVATAR_COLORS.len()])
}

/// Updates a model row by row, so unchanged rows keep their state and animations.
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    for (index, row) in rows.iter().enumerate() {
        if index >= model.row_count() {
            model.push(row.clone());
        } else if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
}

/// A made-up state for looking at the window without a daemon.
fn demo() -> Status {
    let member = |nickname: &str, link, latency_ms, last| MemberStatus {
        nickname: nickname.into(),
        dns: None,
        address: Ipv4Addr::new(100, 64, 0, last),
        link,
        latency_ms,
    };
    Status {
        nickname: "qateralong".into(),
        public_key: String::new(),
        host: None,
        public_link: None,
        servers: vec![ServerStatus {
            server: "weft://141.11.211.11:8443".into(),
            host: "141.11.211.11:8443".into(),
            connection: Connection::Connected,
            latency_ms: Some(38),
            address: Some(Ipv4Addr::new(100, 64, 0, 1)),
            hosted: false,
            public: true,
            networks: vec![
                NetworkStatus {
                    name: "friends".into(),
                    role: Role::Owner,
                    locked: false,
                    approval: true,
                    requests: 1,
                    members: vec![
                        member("alice", PeerLink::Direct, Some(27), 2),
                        member("bob", PeerLink::Relay, None, 3),
                        member("carol", PeerLink::Offline, None, 4),
                    ],
                },
                NetworkStatus {
                    name: "minecraft".into(),
                    role: Role::Member,
                    locked: true,
                    approval: false,
                    requests: 0,
                    members: vec![member("dave", PeerLink::Direct, Some(143), 9)],
                },
            ],
        }],
    }
}
