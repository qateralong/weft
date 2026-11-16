use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RULE: &str = "Weft";
const PROGRAM_RULE: &str = "Weft daemon";
const SERVER_RULE: &str = "Weft server";

/// The address pools of all servers, shared with scripts that run later.
#[derive(Clone)]
pub struct Pools(Arc<Mutex<Vec<String>>>);

impl Pools {
    pub fn new(first: String) -> Self {
        Self(Arc::new(Mutex::new(vec![first])))
    }

    fn list(&self) -> String {
        let pools = self.0.lock().unwrap();
        pools.iter().map(|pool| format!("'{pool}'")).collect::<Vec<_>>().join(",")
    }
}

type Script = Box<dyn FnOnce() -> String + Send>;

/// Runs scripts one at a time in order, so a later one is never overtaken by an earlier one.
fn queue() -> &'static Mutex<Sender<Script>> {
    static QUEUE: OnceLock<Mutex<Sender<Script>>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = channel::<Script>();
        std::thread::spawn(move || {
            for script in rx {
                powershell(&script());
            }
        });
        Mutex::new(tx)
    })
}

fn enqueue(script: impl FnOnce() -> String + Send + 'static) {
    let _ = queue().lock().unwrap().send(Box::new(script));
}

pub fn run_script(script: String) {
    enqueue(move || script);
}

fn powershell(script: &str) {
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    match status {
        Ok(status) if status.success() => tracing::debug!("windows configuration applied"),
        Ok(status) => tracing::warn!(%status, "windows configuration failed"),
        Err(error) => tracing::warn!(%error, "cannot run powershell"),
    }
}

pub fn wintun_path() -> Option<String> {
    let path = std::env::current_exe().ok()?.with_file_name("wintun.dll");
    path.exists().then(|| path.to_string_lossy().into_owned())
}

fn pool_rule(alias: &str, pools: &Pools) -> String {
    format!(
        "Remove-NetFirewallRule -DisplayName '{RULE}'
        New-NetFirewallRule -DisplayName '{RULE}' -Direction Inbound -Action Allow -Profile Any `
            -InterfaceAlias '{alias}' -RemoteAddress @({}) | Out-Null",
        pools.list()
    )
}

/// Marks the adapter as a private network, allows inbound traffic from the pools on it,
/// allows inbound UDP for the daemon so peers can reach it directly, and steers limited
/// broadcast and the given multicast groups into the adapter.
pub fn configure(alias: &str, pools: Pools, routes: &[crate::Route], dns: Option<&crate::Dns>) {
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
    if let Some(dns) = dns {
        extra.push_str(&crate::dns::script(dns));
    }
    enqueue(move || {
        format!(
            "$ErrorActionPreference = 'SilentlyContinue'
        for ($i = 0; $i -lt 30; $i++) {{
            if (Get-NetConnectionProfile -InterfaceAlias '{alias}') {{
                Set-NetConnectionProfile -InterfaceAlias '{alias}' -NetworkCategory Private
                break
            }}
            Start-Sleep -Seconds 1
        }}
        {}
        Remove-NetFirewallRule -DisplayName '{PROGRAM_RULE}'
        New-NetFirewallRule -DisplayName '{PROGRAM_RULE}' -Direction Inbound -Action Allow -Profile Any `
            -Program '{program}' -Protocol UDP | Out-Null{extra}",
            pool_rule(&alias, &pools)
        )
    });
}

/// Lets in the peers of a pool added or removed later.
pub fn allow_pools(alias: &str, pools: Pools, added: Option<String>, removed: Option<String>) {
    {
        let mut list = pools.0.lock().unwrap();
        if let Some(pool) = added.filter(|pool| !list.contains(pool)) {
            list.push(pool);
        }
        if let Some(pool) = removed {
            list.retain(|known| *known != pool);
        }
    }
    let alias = alias.replace('\'', "''");
    enqueue(move || format!("$ErrorActionPreference = 'SilentlyContinue'\n        {}", pool_rule(&alias, &pools)));
}

/// Removes the firewall rules and the resolver rule, for an uninstaller; waits until done.
pub fn cleanup() {
    powershell(&format!(
        "$ErrorActionPreference = 'SilentlyContinue'
        Remove-NetFirewallRule -DisplayName '{RULE}'
        Remove-NetFirewallRule -DisplayName '{PROGRAM_RULE}'
        Remove-NetFirewallRule -DisplayName '{SERVER_RULE}'
        Get-DnsClientNrptRule | Where-Object {{ $_.Namespace -eq '.{}' }} | Remove-DnsClientNrptRule -Force",
        crate::dns::DOMAIN
    ));
}
