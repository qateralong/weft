use std::net::Ipv4Addr;

/// Sends queries for `domain` to `server` through the system resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dns {
    pub server: Ipv4Addr,
    pub domain: String,
}

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
