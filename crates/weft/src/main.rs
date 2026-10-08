use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Arg, ArgAction, ArgMatches, Command};
use weft_i18n::Localizer;
use weft_ipc::{Connection, DeviceInfo, Failure, InviteInfo, PeerLink, Request, Response, Role, Status};

const MAX_EXPIRY: u64 = 365 * 24 * 3600;

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
    let network = || positional(l, "network", "arg-network").required(true);
    let member = || positional(l, "member", "arg-member").required(true);
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
        .subcommand(command(
            l,
            "join",
            "cmd-join",
            vec![positional(l, "target", "arg-target").required(true), password()],
        ))
        .subcommand(command(l, "leave", "cmd-leave", vec![name()]))
        .subcommand(command(l, "status", "cmd-status", vec![]))
        .subcommand(
            command(l, "invite", "cmd-invite", vec![])
                .subcommand_required(true)
                .subcommand_help_heading(l.tr("help-commands"))
                .subcommand(
                    command(
                        l,
                        "create",
                        "cmd-invite-create",
                        vec![network(), option(l, "uses", 'u', "arg-uses"), option(l, "expires", 'e', "arg-expires")],
                    )
                    .override_usage(l.tr("usage-invite-create")),
                )
                .subcommand(
                    command(l, "list", "cmd-invite-list", vec![network()]).override_usage(l.tr("usage-invite-list")),
                )
                .subcommand(
                    command(l, "revoke", "cmd-invite-revoke", vec![positional(l, "code", "arg-code").required(true)])
                        .override_usage(l.tr("usage-invite-revoke")),
                ),
        )
        .subcommand(command(l, "kick", "cmd-kick", vec![network(), member()]))
        .subcommand(command(l, "ban", "cmd-ban", vec![network(), member()]))
        .subcommand(command(l, "unban", "cmd-unban", vec![network(), member()]))
        .subcommand(command(l, "bans", "cmd-bans", vec![network()]))
        .subcommand(command(l, "requests", "cmd-requests", vec![network()]))
        .subcommand(command(l, "approve", "cmd-approve", vec![network(), member()]))
        .subcommand(command(l, "deny", "cmd-deny", vec![network(), member()]))
        .subcommand(command(l, "promote", "cmd-promote", vec![network(), member()]))
        .subcommand(command(l, "demote", "cmd-demote", vec![network(), member()]))
        .subcommand(command(l, "lock", "cmd-lock", vec![network()]))
        .subcommand(command(l, "unlock", "cmd-unlock", vec![network()]))
        .subcommand(command(
            l,
            "approval",
            "cmd-approval",
            vec![network(), positional(l, "mode", "arg-mode").required(true).value_parser(["on", "off"])],
        ))
        .subcommand(command(l, "password", "cmd-password", vec![network(), password()]))
        .subcommand(command(
            l,
            "delete",
            "cmd-delete",
            vec![
                network(),
                Arg::new("yes")
                    .long("yes")
                    .short('y')
                    .action(ArgAction::SetTrue)
                    .help(l.tr("arg-yes"))
                    .help_heading(l.tr("help-options")),
            ],
        ))
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
            let target = arg(m, "target").unwrap_or_default();
            if target.trim_start().to_ascii_lowercase().starts_with("weft://") {
                (Request::Redeem { link: target.trim().to_string() }, None)
            } else {
                let password = password(l, arg(m, "password"), false)?;
                let done = l.tr_args("done-join", &[("name", &target)]);
                (Request::Join { name: target, password }, Some(done))
            }
        }
        Some(("leave", m)) => {
            let name = arg(m, "name").unwrap_or_default();
            let done = l.tr_args("done-leave", &[("name", &name)]);
            (Request::Leave { name }, Some(done))
        }
        Some(("invite", m)) => match m.subcommand() {
            Some(("create", m)) => {
                let uses = arg(m, "uses")
                    .map(|uses| {
                        uses.trim().parse::<u32>().ok().filter(|&uses| uses > 0).ok_or(l.tr("error-invalid-uses"))
                    })
                    .transpose()?;
                let expires_in = arg(m, "expires")
                    .map(|text| parse_duration(&text).ok_or(l.tr("error-invalid-duration")))
                    .transpose()?;
                (Request::CreateInvite { network: arg(m, "network").unwrap_or_default(), uses, expires_in }, None)
            }
            Some(("list", m)) => (Request::Invites { network: arg(m, "network").unwrap_or_default() }, None),
            Some(("revoke", m)) => {
                let code = arg(m, "code").unwrap_or_default();
                let done = l.tr_args("done-revoke", &[("code", &code.to_uppercase())]);
                (Request::RevokeInvite { code }, Some(done))
            }
            _ => (Request::Status, None),
        },
        Some((action @ ("kick" | "ban" | "unban"), m)) => {
            let network = arg(m, "network").unwrap_or_default();
            let member = arg(m, "member").unwrap_or_default();
            let done = l.tr_args(&format!("done-{action}"), &[("name", &network), ("member", &member)]);
            let request = match action {
                "kick" => Request::Kick { network, member },
                "ban" => Request::Ban { network, member },
                _ => Request::Unban { network, member },
            };
            (request, Some(done))
        }
        Some(("bans", m)) => (Request::Bans { network: arg(m, "network").unwrap_or_default() }, None),
        Some(("requests", m)) => (Request::Requests { network: arg(m, "network").unwrap_or_default() }, None),
        Some((action @ ("approve" | "deny" | "promote" | "demote"), m)) => {
            let network = arg(m, "network").unwrap_or_default();
            let member = arg(m, "member").unwrap_or_default();
            let done = l.tr_args(&format!("done-{action}"), &[("name", &network), ("member", &member)]);
            let request = match action {
                "approve" => Request::Approve { network, member },
                "deny" => Request::Deny { network, member },
                "promote" => Request::SetRole { network, member, role: Role::Admin },
                _ => Request::SetRole { network, member, role: Role::Member },
            };
            (request, Some(done))
        }
        Some((action @ ("lock" | "unlock" | "approval" | "password"), m)) => {
            let network = arg(m, "network").unwrap_or_default();
            let (mut locked, mut approval, mut new_password) = (None, None, None);
            let done = match action {
                "lock" | "unlock" => {
                    locked = Some(action == "lock");
                    format!("done-{action}")
                }
                "approval" => {
                    let mode = arg(m, "mode").unwrap_or_default();
                    approval = Some(mode == "on");
                    format!("done-approval-{mode}")
                }
                _ => {
                    new_password = Some(password(l, arg(m, "password"), true)?);
                    "done-password".to_string()
                }
            };
            let done = l.tr_args(&done, &[("name", &network)]);
            (Request::Configure { network, locked, approval, password: new_password }, Some(done))
        }
        Some(("delete", m)) => {
            if !m.get_flag("yes") {
                return Err(l.tr("error-confirm-delete"));
            }
            let network = arg(m, "network").unwrap_or_default();
            let done = l.tr_args("done-delete", &[("name", &network)]);
            (Request::Delete { network }, Some(done))
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
        Response::Joined(name) => {
            println!("{}", l.tr_args("done-join", &[("name", &name)]));
            Ok(())
        }
        Response::Invite(invite) => {
            println!("{}", l.tr_args("done-invite", &[("name", &invite.network)]));
            println!("{}", invite.link.as_deref().unwrap_or(&invite.code));
            println!("{}", invite_details(l, &invite));
            Ok(())
        }
        Response::Invites(invites) => {
            print_invites(l, &request, &invites);
            Ok(())
        }
        Response::Pending(name) => {
            println!("{}", l.tr_args("done-pending", &[("name", &name)]));
            Ok(())
        }
        Response::Bans(devices) | Response::Requests(devices) => {
            print_devices(l, &request, &devices);
            Ok(())
        }
        Response::Error(failure) => Err(l.tr(failure_id(failure))),
    }
}

fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    let unit = match text.chars().last()? {
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    let value: u64 = text[..text.len() - 1].parse().ok()?;
    Some(value.checked_mul(unit)?).filter(|&seconds| (1..=MAX_EXPIRY).contains(&seconds))
}

fn format_remaining(l: &Localizer, seconds: u64) -> String {
    let minutes = seconds.div_ceil(60).max(1);
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    let text = |n: u64| n.to_string();
    if days > 0 {
        l.tr_args("time-days", &[("days", &text(days)), ("hours", &text(hours))])
    } else if hours > 0 {
        l.tr_args("time-hours", &[("hours", &text(hours)), ("minutes", &text(minutes))])
    } else {
        l.tr_args("time-minutes", &[("minutes", &text(minutes))])
    }
}

fn invite_details(l: &Localizer, invite: &InviteInfo) -> String {
    let uses = invite.uses.to_string();
    let uses = match invite.max_uses {
        Some(max) => l.tr_args("invite-uses", &[("uses", &uses), ("max", &max.to_string())]),
        None => l.tr_args("invite-uses-unlimited", &[("uses", &uses)]),
    };
    let expires = match invite.expires {
        Some(expires) => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            l.tr_args("invite-expires", &[("time", &format_remaining(l, expires.saturating_sub(now)))])
        }
        None => l.tr("invite-no-expiry"),
    };
    let by = l.tr_args("invite-by", &[("nickname", &invite.creator)]);
    format!("{uses} · {expires} · {by}")
}

fn print_invites(l: &Localizer, request: &Request, invites: &[InviteInfo]) {
    let Request::Invites { network } = request else { return };
    if invites.is_empty() {
        println!("{}", l.tr_args("invites-empty", &[("name", network)]));
        return;
    }
    println!("{}", l.tr_args("invites-title", &[("name", network)]));
    for invite in invites {
        println!("\n  {}  {}", invite.code, invite_details(l, invite));
        if let Some(link) = &invite.link {
            println!("  {link}");
        }
    }
}

fn print_devices(l: &Localizer, request: &Request, devices: &[DeviceInfo]) {
    let (network, list) = match request {
        Request::Bans { network } => (network, "bans"),
        Request::Requests { network } => (network, "requests"),
        _ => return,
    };
    if devices.is_empty() {
        println!("{}", l.tr_args(&format!("{list}-empty"), &[("name", network)]));
        return;
    }
    println!("{}", l.tr_args(&format!("{list}-title"), &[("name", network)]));
    let width = devices.iter().map(|device| device.nickname.chars().count()).max().unwrap_or(0);
    for device in devices {
        println!("  {:<width$}  {:<15}  {}", device.nickname, device.address.to_string(), device.public_key);
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
        let mut title = l.tr_args("network-title", &[("name", &network.name), ("role", &l.tr(role))]);
        if network.locked {
            title += &format!(" · {}", l.tr("network-locked"));
        }
        if network.approval {
            title += &format!(" · {}", l.tr("network-approval"));
        }
        if network.requests > 0 {
            title += &format!(" · {}", l.tr_args("network-requests", &[("count", &network.requests.to_string())]));
        }
        println!("\n{title}");
        if network.members.is_empty() {
            println!("  {}", l.tr("network-empty"));
        }
        let width = network.members.iter().map(|m| m.nickname.chars().count()).max().unwrap_or(0);
        for member in &network.members {
            let link = match (member.link, member.latency_ms) {
                (PeerLink::Offline, _) => l.tr("link-offline"),
                (PeerLink::Connecting, _) => l.tr("link-connecting"),
                (PeerLink::Relay, _) => l.tr("link-relay"),
                (PeerLink::Direct, Some(ms)) => l.tr_args("link-direct-latency", &[("ms", &ms.to_string())]),
                (PeerLink::Direct, None) => l.tr("link-direct"),
            };
            println!("  {:<width$}  {:<15}  {link}", member.nickname, member.address.to_string());
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
        Failure::Forbidden => "error-forbidden",
        Failure::InviteNotFound => "error-invite-not-found",
        Failure::Banned => "error-banned",
        Failure::MemberNotFound => "error-member-not-found",
        Failure::AmbiguousMember => "error-ambiguous-member",
        Failure::TooManyInvites => "error-too-many-invites",
        Failure::NetworkLocked => "error-network-locked",
        Failure::Internal => "error-internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration(" 12H "), Some(43_200));
        assert_eq!(parse_duration("365d"), Some(MAX_EXPIRY));
        for invalid in ["", "d", "7", "0h", "366d", "-1d", "1.5h", "7w"] {
            assert_eq!(parse_duration(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn remaining_time() {
        let l = Localizer::new(weft_i18n::Language::English);
        assert_eq!(format_remaining(&l, 7 * 86_400 - 30), "7 d 0 h");
        assert_eq!(format_remaining(&l, 3 * 3600 + 120), "3 h 2 min");
        assert_eq!(format_remaining(&l, 5), "1 min");
    }

    #[test]
    fn command_line_is_valid() {
        cli(&Localizer::new(weft_i18n::Language::Russian)).debug_assert();
        cli(&Localizer::new(weft_i18n::Language::English)).debug_assert();
    }
}
