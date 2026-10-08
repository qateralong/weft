use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use loom::admin::{AdminRequest, AdminResponse, DeviceInfo, Stats};
use loom::relay::Traffic;

use crate::AdminCommand;

type BoxError = Box<dyn std::error::Error>;

pub fn run(socket: &Path, action: AdminCommand) -> Result<(), BoxError> {
    let (request, filter) = match action {
        AdminCommand::Stats => (AdminRequest::Stats, None),
        AdminCommand::Networks => (AdminRequest::Networks, None),
        AdminCommand::Devices { online, blocked } => (AdminRequest::Devices, Some((online, blocked))),
        AdminCommand::Block { device } => (AdminRequest::Block { device }, None),
        AdminCommand::Unblock { device } => (AdminRequest::Unblock { device }, None),
        AdminCommand::DeleteNetwork { name, yes } => {
            if !yes {
                return Err("this deletes the network for all its members; add --yes to confirm".into());
            }
            (AdminRequest::DeleteNetwork { name }, None)
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let response = runtime
        .block_on(loom::admin::request(socket, &request))
        .map_err(|error| format!("cannot reach the running loom at {}: {error}", socket.display()))?;
    match response {
        AdminResponse::Ok => println!("done"),
        AdminResponse::Error(error) => return Err(error.into()),
        AdminResponse::Stats(stats) => print_stats(&stats),
        AdminResponse::Networks(networks) => {
            println!("{:<24} {:>7} {:>6}  {:<16} FLAGS", "NAME", "MEMBERS", "ONLINE", "OWNER");
            for network in networks {
                let mut flags = Vec::new();
                if network.locked {
                    flags.push("locked");
                }
                if network.approval {
                    flags.push("approval");
                }
                println!(
                    "{:<24} {:>7} {:>6}  {:<16} {}",
                    network.name,
                    network.members,
                    network.online,
                    network.owner.as_deref().unwrap_or("-"),
                    flags.join(",")
                );
            }
        }
        AdminResponse::Devices(devices) => {
            let (online, blocked) = filter.unwrap_or_default();
            let devices = devices.iter().filter(|d| (!online || d.online) && (!blocked || d.blocked));
            print_devices(devices);
        }
    }
    Ok(())
}

fn print_stats(stats: &Stats) {
    let limit = |mbit: u32| if mbit == 0 { "none".to_string() } else { format!("{mbit} Mbit/s") };
    println!("uptime        {}", duration(stats.uptime_secs));
    println!("devices       {} ({} online, {} blocked)", stats.devices, stats.online, stats.blocked);
    println!("networks      {}", stats.networks);
    println!("relay         {}", traffic(&stats.relay));
    println!("relay limits  {} per device, {} total", limit(stats.relay_mbit), limit(stats.relay_total_mbit));
    if !stats.top_relay.is_empty() {
        println!("\ntop relay users since start");
        print_devices(stats.top_relay.iter());
    }
}

fn print_devices<'a>(devices: impl Iterator<Item = &'a DeviceInfo>) {
    println!("{:<20} {:<15} {:<10} {:>8}  {:<24} KEY", "NICKNAME", "ADDRESS", "STATE", "NETWORKS", "RELAY");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    for device in devices {
        let state = if device.blocked {
            "blocked".to_string()
        } else if device.online {
            "online".to_string()
        } else {
            format!("{} ago", duration((now - device.last_seen).max(0) as u64))
        };
        println!(
            "{:<20} {:<15} {:<10} {:>8}  {:<24} {}",
            device.nickname,
            device.address.to_string(),
            state,
            device.networks,
            if device.relay.packets == 0 { "-".to_string() } else { traffic(&device.relay) },
            device.key
        );
    }
}

fn traffic(traffic: &Traffic) -> String {
    let mut text = format!("{} in {} packets", bytes(traffic.bytes), traffic.packets);
    if traffic.dropped > 0 {
        text += &format!(", {} dropped", traffic.dropped);
    }
    text
}

fn bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", units[unit]) }
}

fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h {}m", secs / 3600, secs / 60 % 60),
        _ => format!("{}d {}h", secs / 86_400, secs / 3600 % 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_durations() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(3 * 3600 + 120), "3h 2m");
        assert_eq!(duration(2 * 86_400 + 3600), "2d 1h");
    }
}
