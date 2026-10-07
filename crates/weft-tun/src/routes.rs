#[cfg(not(windows))]
use std::process::Command;

use crate::Route;

#[cfg(target_os = "linux")]
fn command(name: &str, route: &Route) -> Command {
    let mut command = Command::new("ip");
    command.args(["route", "replace", &format!("{}/{}", route.destination, route.prefix), "dev", name, "metric", "0"]);
    command
}

#[cfg(target_os = "macos")]
fn command(name: &str, route: &Route) -> Command {
    let mut command = Command::new("route");
    command.args(["-n", "add", "-net", &format!("{}/{}", route.destination, route.prefix), "-interface", name]);
    command
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn command(_name: &str, _route: &Route) -> Command {
    Command::new("true")
}

#[cfg(not(windows))]
pub fn apply(name: &str, routes: &[Route]) {
    for route in routes {
        match command(name, route).output() {
            Ok(output) if output.status.success() => tracing::debug!(?route, "route added"),
            Ok(output) => tracing::warn!(
                ?route,
                error = %String::from_utf8_lossy(&output.stderr).trim(),
                "cannot add route"
            ),
            Err(error) => tracing::warn!(?route, %error, "cannot add route"),
        }
    }
}

#[cfg(windows)]
pub fn describe(routes: &[Route]) -> Vec<String> {
    routes.iter().map(|route| format!("{}/{}", route.destination, route.prefix)).collect()
}
