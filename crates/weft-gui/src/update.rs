//! Looks for a newer release on GitHub a few times a day.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::settings::Release;

const LATEST: &str = "https://api.github.com/repos/qateralong/weft/releases/latest";
pub const RELEASES: &str = "https://github.com/qateralong/weft/releases/";
pub const EVERY: Duration = Duration::from_secs(6 * 3600);

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// Fetches the latest release; runs blocking, so call it off the UI thread.
pub fn fetch() -> Option<Release> {
    let mut response = ureq::get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("weft-gui/", env!("CARGO_PKG_VERSION")))
        .call()
        .ok()?;
    let body = response.body_mut().read_to_string().ok()?;
    let release: serde_json::Value = serde_json::from_str(&body).ok()?;
    Some(Release {
        tag: release.get("tag_name")?.as_str()?.to_string(),
        url: release.get("html_url")?.as_str()?.to_string(),
        checked: now(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("v999.0.0"));
        assert!(!newer("v0.0.1"));
        assert!(!newer(concat!("v", env!("CARGO_PKG_VERSION"))));
    }
}
