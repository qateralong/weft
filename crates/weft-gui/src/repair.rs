//! Starts the daemon and grants this user access, asking the system for administrator rights.

use std::process::Command;

/// Runs the fix and waits for it; the error is shown to the user as is.
pub fn run() -> Result<(), String> {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    if !user.chars().all(|c| c.is_alphanumeric() || "._-".contains(c)) {
        return Err(format!("unexpected user name {user}"));
    }
    let status = command(&user).status().map_err(|error| error.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("{status}")) }
}

#[cfg(target_os = "linux")]
fn command(user: &str) -> Command {
    let script = "getent group weft > /dev/null || groupadd --system weft
        usermod -aG weft \"$1\"
        systemctl enable --now weftd.service";
    let mut command = Command::new("pkexec");
    command.args(["/bin/sh", "-c", script, "sh", user]);
    command
}

#[cfg(target_os = "macos")]
fn command(user: &str) -> Command {
    let plist = "/Library/LaunchDaemons/org.weft.weftd.plist";
    let script = format!(
        "do shell script \"(dseditgroup -o read weft > /dev/null || dseditgroup -o create -r Weft weft); \
         dseditgroup -o edit -a {user} -t user weft; \
         launchctl bootstrap system {plist} 2> /dev/null; launchctl kickstart system/org.weft.weftd\" \
         with administrator privileges"
    );
    let mut command = Command::new("osascript");
    command.args(["-e", &script]);
    command
}

#[cfg(windows)]
fn command(_user: &str) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let inner = "Set-Service weftd -StartupType Automatic; Start-Service weftd";
    let outer = format!(
        "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-Command','{inner}'"
    );
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &outer]).creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn command(_user: &str) -> Command {
    Command::new("false")
}
