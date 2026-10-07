use std::io;

#[cfg(windows)]
pub const NAME: &str = "weftd";
const DESCRIPTION: &str = "Weft peer-to-peer virtual LAN";

#[cfg(unix)]
fn run(program: &str, args: &[&str]) -> io::Result<()> {
    let status = std::process::Command::new(program).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} {} failed: {status}", args.join(" "))))
    }
}

#[cfg(unix)]
fn executable() -> io::Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
const UNIT: &str = "/etc/systemd/system/weftd.service";

#[cfg(target_os = "linux")]
pub fn install() -> io::Result<()> {
    let unit = format!(
        "[Unit]\nDescription={DESCRIPTION}\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nExecStart={}\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n",
        executable()?
    );
    std::fs::write(UNIT, unit)?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "--now", "weftd.service"])
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> io::Result<()> {
    let _ = run("systemctl", &["disable", "--now", "weftd.service"]);
    match std::fs::remove_file(UNIT) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    run("systemctl", &["daemon-reload"])
}

#[cfg(target_os = "macos")]
const LABEL: &str = "org.weft.weftd";
#[cfg(target_os = "macos")]
const PLIST: &str = "/Library/LaunchDaemons/org.weft.weftd.plist";

#[cfg(target_os = "macos")]
pub fn install() -> io::Result<()> {
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array><string>{}</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardErrorPath</key><string>/var/log/weftd.log</string>
</dict>
</plist>
"#,
        executable()?
    );
    std::fs::write(PLIST, plist)?;
    run("launchctl", &["bootstrap", "system", PLIST])
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> io::Result<()> {
    let _ = run("launchctl", &["bootout", &format!("system/{LABEL}")]);
    match std::fs::remove_file(PLIST) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn install() -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn uninstall() -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(windows)]
pub fn install() -> io::Result<()> {
    use std::ffi::OsString;

    use windows_service::service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).map_err(io::Error::other)?;
    let info = ServiceInfo {
        name: OsString::from(NAME),
        display_name: OsString::from("Weft"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service =
        manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START).map_err(io::Error::other)?;
    service.set_description(DESCRIPTION).map_err(io::Error::other)?;
    service.start::<&str>(&[]).map_err(io::Error::other)
}

#[cfg(windows)]
pub fn uninstall() -> io::Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(io::Error::other)?;
    let service = manager
        .open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)
        .map_err(io::Error::other)?;
    if service.query_status().map_err(io::Error::other)?.current_state != ServiceState::Stopped {
        let _ = service.stop();
    }
    service.delete().map_err(io::Error::other)
}

#[cfg(windows)]
pub mod windows {
    use std::ffi::OsString;
    use std::sync::OnceLock;
    use std::time::Duration;

    use tokio::sync::watch;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::NAME;

    type Runner = Box<dyn FnOnce(watch::Receiver<bool>) -> std::io::Result<()> + Send>;

    static RUNNER: OnceLock<std::sync::Mutex<Option<Runner>>> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    pub fn dispatch(runner: Runner) -> std::io::Result<()> {
        RUNNER.get_or_init(|| std::sync::Mutex::new(Some(runner)));
        service_dispatcher::start(NAME, ffi_service_main).map_err(std::io::Error::other)
    }

    fn service_main(_arguments: Vec<OsString>) {
        let (stop_tx, stop_rx) = watch::channel(false);
        let handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let Ok(status) = service_control_handler::register(NAME, handler) else { return };
        let report = |state, accept, code| {
            let _ = status.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: accept,
                exit_code: ServiceExitCode::Win32(code),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            });
        };
        report(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, 0);
        let runner = RUNNER.get().and_then(|cell| cell.lock().ok()?.take());
        let code = match runner.map(|run| run(stop_rx)) {
            Some(Ok(())) => 0,
            _ => 1,
        };
        report(ServiceState::Stopped, ServiceControlAccept::empty(), code);
    }
}
