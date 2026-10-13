//! Looks for a newer release on GitHub at start and every hour, and installs it with the
//! system's own installer.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::settings::{Asset, Release};

const LATEST: &str = "https://api.github.com/repos/qateralong/weft/releases/latest";
pub const RELEASES: &str = "https://github.com/qateralong/weft/releases/";
const DOWNLOADS: &str = "https://github.com/qateralong/weft/releases/download/";
pub const EVERY: Duration = Duration::from_secs(3600);

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

fn agent() -> &'static str {
    concat!("weft-gui/", env!("CARGO_PKG_VERSION"))
}

/// Fetches the latest release; runs blocking, so call it off the UI thread.
pub fn fetch() -> Option<Release> {
    let mut response =
        ureq::get(LATEST).header("Accept", "application/vnd.github+json").header("User-Agent", agent()).call().ok()?;
    let body = response.body_mut().read_to_string().ok()?;
    let release: serde_json::Value = serde_json::from_str(&body).ok()?;
    let assets = release
        .get("assets")
        .and_then(|assets| assets.as_array())
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    Some(Asset {
                        name: asset.get("name")?.as_str()?.to_string(),
                        url: asset.get("browser_download_url")?.as_str()?.to_string(),
                        sha256: asset
                            .get("digest")
                            .and_then(|digest| digest.as_str())
                            .and_then(|digest| digest.strip_prefix("sha256:"))
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release {
        tag: release.get("tag_name")?.as_str()?.to_string(),
        url: release.get("html_url")?.as_str()?.to_string(),
        checked: now(),
        assets,
    })
}

/// Whether `tag` (like `v1.2.3`) is newer than this build.
pub fn newer(tag: &str) -> bool {
    let parse = |text: &str| -> Vec<u64> {
        text.trim_start_matches('v').split('.').map(|part| part.parse().unwrap_or(0)).collect()
    };
    let (theirs, ours) = (parse(tag), parse(env!("CARGO_PKG_VERSION")));
    (0..3)
        .map(|i| (theirs.get(i).copied().unwrap_or(0), ours.get(i).copied().unwrap_or(0)))
        .find(|(a, b)| a != b)
        .is_some_and(|(a, b)| a > b)
}

/// How a downloaded package gets installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installer {
    Msi,
    Pkg,
    Pacman,
    Apt,
    Dnf,
    Zypper,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub asset: Asset,
    pub installer: Installer,
}

/// The package of `release` for this system, when the app can install it by itself.
pub fn plan(release: &Release) -> Option<Plan> {
    let installer = installer()?;
    let arch = std::env::consts::ARCH;
    let wanted = |name: &str| match installer {
        Installer::Msi => arch == "x86_64" && name.ends_with("-x64.msi"),
        Installer::Pkg => name.ends_with(&format!("-{}.pkg", if arch == "aarch64" { "arm64" } else { arch })),
        Installer::Pacman => is_app(name, "weft-") && name.ends_with(&format!("-{arch}.pkg.tar.zst")),
        Installer::Apt => {
            is_app(name, "weft_") && name.ends_with(if arch == "aarch64" { "_arm64.deb" } else { "_amd64.deb" })
        }
        Installer::Dnf | Installer::Zypper => is_app(name, "weft-") && name.ends_with(&format!(".{arch}.rpm")),
    };
    let asset = release.assets.iter().find(|asset| wanted(&asset.name) && asset.url.starts_with(DOWNLOADS))?;
    Some(Plan { asset: asset.clone(), installer })
}

/// The app's package rather than the server's.
fn is_app(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix) && !name.starts_with("weft-loom") && !name.starts_with("weft_loom")
}

fn installer() -> Option<Installer> {
    if cfg!(windows) {
        return Some(Installer::Msi);
    }
    if cfg!(target_os = "macos") {
        return Some(Installer::Pkg);
    }
    if !cfg!(target_os = "linux") || !Path::new("/usr/bin/pkexec").exists() {
        return None;
    }
    [
        ("/usr/bin/pacman", Installer::Pacman),
        ("/usr/bin/apt-get", Installer::Apt),
        ("/usr/bin/dnf", Installer::Dnf),
        ("/usr/bin/zypper", Installer::Zypper),
    ]
    .into_iter()
    .find(|(path, _)| Path::new(path).exists())
    .map(|(_, installer)| installer)
}

/// Downloads the package next to other caches, checking its SHA-256 when GitHub gave one.
pub fn download(asset: &Asset, progress: impl Fn(u8)) -> Result<PathBuf, String> {
    let dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("weft").join("updates");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let name = Path::new(&asset.name).file_name().ok_or("bad file name")?;
    let path = dir.join(name);
    let mut response = ureq::get(&asset.url).header("User-Agent", agent()).call().map_err(|error| error.to_string())?;
    let total: Option<u64> =
        response.headers().get("content-length").and_then(|value| value.to_str().ok()?.parse().ok());
    let mut reader = response.body_mut().with_config().limit(512 * 1024 * 1024).reader();
    let mut file = std::fs::File::create(&path).map_err(|error| error.to_string())?;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let (mut buffer, mut done, mut shown) = (vec![0; 64 * 1024], 0u64, 0u8);
    loop {
        let read = reader.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
        digest.update(&buffer[..read]);
        done += read as u64;
        if let Some(total) = total.filter(|&total| total > 0) {
            let percent = (done * 100 / total).min(100) as u8;
            if percent != shown {
                shown = percent;
                progress(percent);
            }
        }
    }
    file.sync_all().map_err(|error| error.to_string())?;
    let actual: String = digest.finish().as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    if asset.sha256.as_ref().is_some_and(|expected| !expected.eq_ignore_ascii_case(&actual)) {
        let _ = std::fs::remove_file(&path);
        return Err("checksum mismatch".into());
    }
    Ok(path)
}

/// Starts the installer. On Windows it runs on after this app quits and starts the new version
/// when done; elsewhere this waits for it, and a Linux package update restarts the app.
pub fn install(plan: &Plan, path: &Path) -> Result<(), String> {
    let mut command = match plan.installer {
        Installer::Msi => {
            let mut command = Command::new("msiexec");
            command.arg("/i").arg(path).args(["/passive", "/norestart"]);
            return command.spawn().map(drop).map_err(|error| error.to_string());
        }
        Installer::Pkg => {
            let mut command = Command::new("open");
            command.arg(path);
            command
        }
        Installer::Pacman => pkexec(&["pacman", "-U", "--noconfirm"], path),
        Installer::Apt => pkexec(&["apt-get", "install", "-y", "--allow-downgrades"], path),
        Installer::Dnf => pkexec(&["dnf", "install", "-y"], path),
        Installer::Zypper => pkexec(&["zypper", "--non-interactive", "install", "--allow-unsigned-rpm"], path),
    };
    let status = command.status().map_err(|error| error.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("{status}")) }
}

fn pkexec(args: &[&str], path: &Path) -> Command {
    let mut command = Command::new("pkexec");
    command.args(args).arg(path);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("v999.0.0"));
        assert!(!newer("v0.0.1"));
        assert!(!newer(concat!("v", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn picks_the_app_package() {
        let asset = |name: &str| Asset { name: name.into(), url: format!("{DOWNLOADS}v9.0.0/{name}"), sha256: None };
        let release = Release {
            tag: "v9.0.0".into(),
            url: String::new(),
            checked: 0,
            assets: [
                "weft-loom-9.0.0-1-x86_64.pkg.tar.zst",
                "weft-9.0.0-1-x86_64.pkg.tar.zst",
                "weft-loom_9.0.0_amd64.deb",
                "weft_9.0.0_amd64.deb",
                "weft-loom-9.0.0-1.x86_64.rpm",
                "weft-9.0.0-1.x86_64.rpm",
                "weft-9.0.0-x64.msi",
                "weft-9.0.0-arm64.pkg",
            ]
            .map(asset)
            .to_vec(),
        };
        if let Some(plan) = plan(&release) {
            assert!(!plan.asset.name.contains("loom"));
        }
        let names: Vec<&str> = release.assets.iter().map(|asset| asset.name.as_str()).collect();
        assert!(is_app(names[1], "weft-") && !is_app(names[0], "weft-") && !is_app(names[2], "weft_"));
    }
}
