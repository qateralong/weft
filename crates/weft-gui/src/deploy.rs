//! Installs the Weft server on a Linux machine over SSH, with only its address, login and password.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use russh::ChannelMsg;
use russh::client::{self, KeyboardInteractiveAuthResponse};
use russh::keys::PublicKeyOrCertificate;

const SCRIPT: &str = include_str!("install-loom.sh");
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Where the server downloads the binary from; the latest release when unset.
    pub binary: Option<String>,
}

/// Why the installation failed; `Script` carries the code the script printed.
#[derive(Debug)]
pub enum DeployError {
    Unreachable(String),
    /// The server's key differs from the one remembered: someone may be in the middle.
    HostKeyChanged,
    Login,
    Script(String),
    Other(String),
}

/// Checks the server's key like ssh does: against the user's known_hosts, then against the keys
/// this app met before, and remembers a new server's key on first use. A changed key stops the
/// connection before the password is sent.
struct KnownHosts {
    host: String,
    port: u16,
    changed: Arc<AtomicBool>,
}

impl KnownHosts {
    fn own_file() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("weft").join("known_hosts"))
    }

    fn system_file() -> Option<PathBuf> {
        dirs::home_dir().map(|dir| dir.join(".ssh").join("known_hosts"))
    }

    fn verify(&self, key: &russh::keys::PublicKey) -> bool {
        use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
        let check = |path: Option<PathBuf>| match path {
            Some(path) if path.exists() => check_known_hosts_path(&self.host, self.port, key, path),
            _ => Ok(false),
        };
        for file in [Self::system_file(), Self::own_file()] {
            match check(file) {
                Ok(true) => return true,
                Err(russh::keys::Error::KeyChanged { .. }) => {
                    self.changed.store(true, Ordering::Relaxed);
                    return false;
                }
                _ => {}
            }
        }
        Self::own_file().is_some_and(|path| learn_known_hosts_path(&self.host, self.port, key, path).is_ok())
    }
}

impl client::Handler for KnownHosts {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => self.verify(key),
            PublicKeyOrCertificate::Certificate(certificate) => {
                self.verify(&russh::keys::PublicKey::new(certificate.public_key().clone(), ""))
            }
        })
    }
}

/// What the installer reports back.
pub struct Deployed {
    pub link: String,
    pub panel: Option<weft_ipc::PanelAccess>,
}

/// Runs the installer and returns the server link; `step` gets the script's progress.
pub async fn deploy(target: &Target, step: impl Fn(&str)) -> Result<Deployed, DeployError> {
    let config = Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(120)), ..Default::default() });
    let host = target.host.trim().trim_start_matches('[').trim_end_matches(']');
    let changed = Arc::new(AtomicBool::new(false));
    let handler = KnownHosts { host: host.to_string(), port: target.port, changed: changed.clone() };
    let connect = client::connect(config, (host, target.port), handler);
    let mut session = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| DeployError::Unreachable("timed out".into()))?
        .map_err(|error| match changed.load(Ordering::Relaxed) {
            true => DeployError::HostKeyChanged,
            false => DeployError::Unreachable(error.to_string()),
        })?;
    login(&mut session, target).await?;

    let root = target.user == "root";
    let mut command = String::from(if root { "bash -s --" } else { "sudo -S -p '' bash -s --" });
    for argument in [Some(&target.host), target.binary.as_ref()].into_iter().flatten() {
        command += &format!(" '{}'", argument.replace('\'', "'\\''"));
    }
    let channel = session.channel_open_session().await.map_err(other)?;
    channel.exec(true, command).await.map_err(other)?;
    let mut input = String::new();
    if !root {
        input.push_str(&target.password);
        input.push('\n');
    }
    input.push_str(SCRIPT);
    channel.data(input.as_bytes()).await.map_err(other)?;
    channel.eof().await.map_err(other)?;

    let mut channel = channel;
    let mut output = String::new();
    let mut link = None;
    let mut panel_url = None;
    let mut panel_password = None;
    let mut error = None;
    let mut status = None;
    let mut handle = |line: &str| {
        if let Some(name) = line.strip_prefix("STEP ") {
            step(name.trim());
        } else if let Some(found) = line.strip_prefix("LINK ") {
            link = Some(found.trim().to_string());
        } else if let Some(found) = line.strip_prefix("PANELPASS ") {
            panel_password = Some(found.trim().to_string());
        } else if let Some(found) = line.strip_prefix("PANEL ") {
            panel_url = Some(found.trim().to_string());
        } else if let Some(code) = line.strip_prefix("ERROR ") {
            error = Some(code.trim().to_string());
        }
    };
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                output.push_str(&String::from_utf8_lossy(&data));
                while let Some(end) = output.find('\n') {
                    let line: String = output.drain(..=end).collect();
                    handle(line.trim_end());
                }
            }
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            _ => {}
        }
    }
    handle(output.trim_end());
    let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;
    let panel = panel_url.zip(panel_password).map(|(url, password)| weft_ipc::PanelAccess { url, password });
    match (link, error, status) {
        (Some(link), _, _) => Ok(Deployed { link, panel }),
        (None, Some(code), _) => Err(DeployError::Script(code)),
        (None, None, status) => Err(DeployError::Other(format!("installer exited with {status:?}"))),
    }
}

async fn login(session: &mut client::Handle<KnownHosts>, target: &Target) -> Result<(), DeployError> {
    let result = session.authenticate_password(&target.user, &target.password).await.map_err(other)?;
    if result.success() {
        return Ok(());
    }
    let mut reply = session.authenticate_keyboard_interactive_start(&target.user, None).await.map_err(other)?;
    for _ in 0..3 {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Err(DeployError::Login),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| target.password.clone()).collect();
                reply = session.authenticate_keyboard_interactive_respond(answers).await.map_err(other)?;
            }
        }
    }
    Err(DeployError::Login)
}

fn other(error: russh::Error) -> DeployError {
    DeployError::Other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs against a disposable machine: WEFT_DEPLOY_TEST="host port user password binary-url".
    #[tokio::test]
    #[ignore]
    async fn deploys_to_a_test_machine() {
        let spec = std::env::var("WEFT_DEPLOY_TEST").expect("WEFT_DEPLOY_TEST");
        let parts: Vec<&str> = spec.split_whitespace().collect();
        let target = Target {
            host: parts[0].into(),
            port: parts[1].parse().unwrap(),
            user: parts[2].into(),
            password: parts[3].into(),
            binary: parts.get(4).map(|url| url.to_string()),
        };
        let steps = std::sync::Mutex::new(Vec::new());
        let deployed = deploy(&target, |step| steps.lock().unwrap().push(step.to_string())).await.unwrap();
        println!("steps {:?} link {} panel {:?}", steps.lock().unwrap(), deployed.link, deployed.panel);
        assert!(deployed.link.starts_with("weft://"));
    }
}
