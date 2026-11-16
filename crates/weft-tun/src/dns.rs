use std::net::Ipv4Addr;

/// Sends queries for `domain` to `server` through the system resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dns {
    pub server: Ipv4Addr,
    pub domain: String,
}

/// The zone the daemon answers, as the uninstaller must know it without the daemon.
pub const DOMAIN: &str = "weft";

/// Stops sending queries to the daemon; per-link settings on Linux go away with the link.
#[cfg(target_os = "macos")]
pub fn remove(dns: &Dns) {
    let _ = std::fs::remove_file(format!("/etc/resolver/{}", dns.domain));
}

#[cfg(windows)]
pub fn remove(dns: &Dns) {
    crate::windows::run_script(format!(
        "Get-DnsClientNrptRule | Where-Object {{ $_.Namespace -eq '.{}' }} | Remove-DnsClientNrptRule -Force",
        dns.domain
    ));
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn remove(_dns: &Dns) {}

#[cfg(target_os = "linux")]
pub fn apply(interface: &str, dns: &Dns) -> bool {
    let run = |args: &[&str]| match std::process::Command::new("resolvectl").args(args).output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            tracing::warn!(error = %String::from_utf8_lossy(&output.stderr).trim(), "resolvectl failed");
            false
        }
        Err(error) => {
            tracing::warn!(%error, "systemd-resolved is not available, peer names will not resolve");
            false
        }
    };
    run(&["dns", interface, &dns.server.to_string()]) && run(&["domain", interface, &format!("~{}", dns.domain)])
}

#[cfg(target_os = "macos")]
pub fn apply(_interface: &str, dns: &Dns) -> bool {
    let result = std::fs::create_dir_all("/etc/resolver")
        .and_then(|()| std::fs::write(format!("/etc/resolver/{}", dns.domain), format!("nameserver {}\n", dns.server)));
    if let Err(error) = &result {
        tracing::warn!(%error, "cannot write /etc/resolver");
    }
    result.is_ok()
}

#[cfg(windows)]
pub fn script(dns: &Dns) -> String {
    format!(
        "\n        Get-DnsClientNrptRule | Where-Object {{ $_.Namespace -eq '.{0}' }} | Remove-DnsClientNrptRule -Force\
         \n        Add-DnsClientNrptRule -Namespace '.{0}' -NameServers '{1}' | Out-Null",
        dns.domain, dns.server
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn apply(_interface: &str, _dns: &Dns) -> bool {
    false
}
