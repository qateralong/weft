use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use weft_i18n::Localizer;
use weft_ipc::{Connection, Failure, PeerLink, Request, Response, Role, Status};

fn main() -> ExitCode {
    let l = Localizer::from_env();
    let matches = cli(&l).get_matches();
    match run(&l, &matches) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn cli(l: &Localizer) -> Command {
    let name = || positional(l, "name", "arg-name").required(true);
    let password = || option(l, "password", 'p', "arg-password");
    command(l, "weft", "app-about", vec![])
        .version(env!("CARGO_PKG_VERSION"))
        .disable_version_flag(true)
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::Version)
                .help(l.tr("arg-version"))
                .help_heading(l.tr("help-options")),
        )
        .subcommand_required(true)
        .disable_help_subcommand(true)
        .subcommand_help_heading(l.tr("help-commands"))
        .subcommand(command(
            l,
            "up",
            "cmd-up",
            vec![positional(l, "link", "arg-link"), option(l, "nickname", 'n', "arg-nickname")],
        ))
        .subcommand(command(l, "down", "cmd-down", vec![]))
        .subcommand(command(l, "create", "cmd-create", vec![name(), password()]))
        .subcommand(command(l, "join", "cmd-join", vec![name(), password()]))
        .subcommand(command(l, "leave", "cmd-leave", vec![name()]))
        .subcommand(command(l, "status", "cmd-status", vec![]))
}

fn command(l: &Localizer, name: &'static str, about: &str, args: Vec<Arg>) -> Command {
    Command::new(name)
        .about(l.tr(about))
        .override_usage(l.tr(&format!("usage-{name}")))
        .help_template(format!("{{about}}\n\n{} {{usage}}\n\n{{all-args}}", l.tr("help-usage")))
        .disable_help_flag(true)
        .args(args)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help(l.tr("arg-help"))
                .help_heading(l.tr("help-options")),
        )
}

fn positional(l: &Localizer, id: &'static str, help: &str) -> Arg {
    Arg::new(id).value_name(l.tr(&format!("value-{id}"))).help(l.tr(help)).help_heading(l.tr("help-arguments"))
}

fn option(l: &Localizer, id: &'static str, short: char, help: &str) -> Arg {
    Arg::new(id)
        .long(id)
        .short(short)
        .value_name(l.tr(&format!("value-{id}")))
        .help(l.tr(help))
        .help_heading(l.tr("help-options"))
}

fn run(l: &Localizer, matches: &ArgMatches) -> Result<(), String> {
    let arg = |m: &ArgMatches, id: &str| m.get_one::<String>(id).cloned();
    let (request, done) = match matches.subcommand() {
        Some(("up", m)) => (Request::Up { link: arg(m, "link"), nickname: arg(m, "nickname") }, Some(l.tr("done-up"))),
        Some(("down", _)) => (Request::Down, Some(l.tr("done-down"))),
        Some(("create", m)) => {
            let name = arg(m, "name").unwrap_or_default();
            let password = password(l, arg(m, "password"), true)?;
            let done = l.tr_args("done-create", &[("name", &name)]);
            (Request::Create { name, password }, Some(done))
        }
        Some(("join", m)) => {
            let name = arg(m, "name").unwrap_or_default();
            let password = password(l, arg(m, "password"), false)?;
            let done = l.tr_args("done-join", &[("name", &name)]);
            (Request::Join { name, password }, Some(done))
        }
        Some(("leave", m)) => {
            let name = arg(m, "name").unwrap_or_default();
            let done = l.tr_args("done-leave", &[("name", &name)]);
            (Request::Leave { name }, Some(done))
        }
        _ => (Request::Status, None),
    };

    let path = weft_ipc::socket_path();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let response = runtime.block_on(weft_ipc::request(&path, &request)).map_err(|error| {
        let path = path.display().to_string();
        match error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                l.tr_args("error-daemon-missing", &[("path", &path)])
            }
            std::io::ErrorKind::PermissionDenied => l.tr_args("error-daemon-permission", &[("path", &path)]),
            _ => l.tr_args("error-daemon", &[("path", &path), ("reason", &error.to_string())]),
        }
    })?;
    match response {
        Response::Ok => {
            if let Some(done) = done {
                println!("{done}");
            }
            Ok(())
        }
        Response::Status(status) => {
            print_status(l, &status);
            Ok(())
        }
        Response::Error(failure) => Err(l.tr(failure_id(failure))),
    }
}

fn password(l: &Localizer, given: Option<String>, confirm: bool) -> Result<String, String> {
    if let Some(password) = given {
        return Ok(password);
    }
    let read = |id| rpassword::prompt_password(format!("{} ", l.tr(id))).map_err(|e| e.to_string());
    let password = read("prompt-password")?;
    if confirm && read("prompt-password-repeat")? != password {
        return Err(l.tr("password-mismatch"));
    }
    Ok(password)
}

fn print_status(l: &Localizer, status: &Status) {
    let state = match status.connection {
        Connection::Disconnected => "state-disconnected",
        Connection::Connecting => "state-connecting",
        Connection::Connected => "state-connected",
    };
    let none = l.tr("status-none");
    let rows = [
        (l.tr("status-server"), status.server.clone().unwrap_or_else(|| none.clone())),
        (l.tr("status-state"), l.tr(state)),
        (l.tr("status-nickname"), status.nickname.clone()),
        (l.tr("status-address"), status.address.map_or_else(|| none.clone(), |a| a.to_string())),
        (l.tr("status-key"), status.public_key.clone()),
    ];
    let width = rows.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0) + 1;
    for (label, value) in &rows {
        println!("{:<width$} {value}", format!("{label}:"), width = width);
    }

    if status.networks.is_empty() {
        if status.connection == Connection::Connected {
            println!("\n{}", l.tr("status-no-networks"));
        }
        return;
    }
    for network in &status.networks {
        let role = match network.role {
            Role::Owner => "role-owner",
            Role::Admin => "role-admin",
            Role::Member => "role-member",
        };
        println!("\n{}", l.tr_args("network-title", &[("name", &network.name), ("role", &l.tr(role))]));
        if network.members.is_empty() {
            println!("  {}", l.tr("network-empty"));
        }
        let width = network.members.iter().map(|m| m.nickname.chars().count()).max().unwrap_or(0);
        for member in &network.members {
            let link = match member.link {
                PeerLink::Offline => "link-offline",
                PeerLink::Connecting => "link-connecting",
                PeerLink::Direct => "link-direct",
            };
            println!("  {:<width$}  {:<15}  {}", member.nickname, member.address.to_string(), l.tr(link));
        }
    }
}

fn failure_id(failure: Failure) -> &'static str {
    match failure {
        Failure::NoServer => "error-no-server",
        Failure::InvalidLink => "error-invalid-link",
        Failure::NotConnected => "error-not-connected",
        Failure::Timeout => "error-timeout",
        Failure::UnsupportedVersion => "error-unsupported-version",
        Failure::InvalidRequest => "error-invalid-request",
        Failure::InvalidName => "error-invalid-name",
        Failure::InvalidNickname => "error-invalid-nickname",
        Failure::InvalidPassword => "error-invalid-password",
        Failure::NetworkExists => "error-network-exists",
        Failure::NetworkNotFound => "error-network-not-found",
        Failure::WrongPassword => "error-wrong-password",
        Failure::NetworkFull => "error-network-full",
        Failure::RateLimited => "error-rate-limited",
        Failure::AlreadyMember => "error-already-member",
        Failure::NotMember => "error-not-member",
        Failure::PoolExhausted => "error-pool-exhausted",
        Failure::Internal => "error-internal",
    }
}
