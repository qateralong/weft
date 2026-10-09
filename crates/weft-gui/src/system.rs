//! Clipboard, links, folders and restarts, done the way each system expects.

use std::cell::RefCell;
use std::path::PathBuf;

thread_local! {
    // On Linux the copied text lives only as long as its owner, so the owner is kept around.
    static CLIPBOARD: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };
}

pub fn copy(text: &str) -> Result<(), String> {
    CLIPBOARD.with(|clipboard| {
        let mut clipboard = clipboard.borrow_mut();
        if clipboard.is_none() {
            *clipboard = Some(arboard::Clipboard::new().map_err(|error| error.to_string())?);
        }
        clipboard.as_mut().map_or(Ok(()), |clipboard| clipboard.set_text(text).map_err(|error| error.to_string()))
    })
}

pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").creation_flags(0x0800_0000);
        command
    };
    command.arg(url).spawn().map(|_| ()).map_err(|error| error.to_string())
}

pub fn downloads_dir() -> PathBuf {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(std::env::temp_dir)
}

/// After a package update the running binary is gone; starts the new one in its place and
/// reports whether this process should quit.
#[cfg(target_os = "linux")]
pub fn restart_if_replaced(visible: bool) -> bool {
    let Ok(exe) = std::fs::read_link("/proc/self/exe") else { return false };
    let Some(path) = exe.to_str().and_then(|path| path.strip_suffix(" (deleted)")) else { return false };
    if !std::path::Path::new(path).exists() {
        return false;
    }
    let hidden = if visible { "" } else { " --hidden" };
    let script = format!("sleep 1; exec '{}'{hidden}", path.replace('\'', ""));
    std::process::Command::new("setsid").args(["-f", "sh", "-c", &script]).spawn().is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn restart_if_replaced(_visible: bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore]
    fn copies_to_the_desktop_clipboard() {
        super::copy("weft-clipboard-check").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let pasted = std::process::Command::new("wl-paste").arg("-n").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&pasted.stdout), "weft-clipboard-check");
    }
}
