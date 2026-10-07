use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RULE: &str = "Weft";
const PROGRAM_RULE: &str = "Weft daemon";

pub fn wintun_path() -> Option<String> {
    let path = std::env::current_exe().ok()?.with_file_name("wintun.dll");
    path.exists().then(|| path.to_string_lossy().into_owned())
}

/// Marks the adapter as a private network, allows inbound traffic from the pool on it,
/// allows inbound UDP for the daemon so peers can reach it directly, and steers limited
/// broadcast and the given multicast groups into the adapter.
pub fn configure(alias: &str, network: &str, routes: &[crate::Route]) {
    let alias = alias.replace('\'', "''");
    let program = std::env::current_exe().map(|path| path.to_string_lossy().replace('\'', "''")).unwrap_or_default();
    let mut extra = String::new();
    for route in crate::routes::describe(routes) {
        if route == "255.255.255.255/32" {
            extra.push_str(&format!("\n        Set-NetIPInterface -InterfaceAlias '{alias}' -InterfaceMetric 1"));
        } else {
            extra.push_str(&format!(
                "\n        New-NetRoute -DestinationPrefix '{route}' -InterfaceAlias '{alias}' -RouteMetric 0 -PolicyStore ActiveStore | Out-Null"
            ));
        }
    }
    let script = format!(
        "$ErrorActionPreference = 'SilentlyContinue'
        for ($i = 0; $i -lt 30; $i++) {{
            if (Get-NetConnectionProfile -InterfaceAlias '{alias}') {{
                Set-NetConnectionProfile -InterfaceAlias '{alias}' -NetworkCategory Private
                break
            }}
            Start-Sleep -Seconds 1
        }}
        Remove-NetFirewallRule -DisplayName '{RULE}'
        New-NetFirewallRule -DisplayName '{RULE}' -Direction Inbound -Action Allow -Profile Any `
            -InterfaceAlias '{alias}' -RemoteAddress '{network}' | Out-Null
        Remove-NetFirewallRule -DisplayName '{PROGRAM_RULE}'
        New-NetFirewallRule -DisplayName '{PROGRAM_RULE}' -Direction Inbound -Action Allow -Profile Any `
            -Program '{program}' -Protocol UDP | Out-Null{extra}"
    );
    std::thread::spawn(move || {
        let status = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        match status {
            Ok(status) if status.success() => tracing::info!("windows firewall configured"),
            Ok(status) => tracing::warn!(%status, "windows firewall configuration failed"),
            Err(error) => tracing::warn!(%error, "cannot run powershell"),
        }
    });
}
