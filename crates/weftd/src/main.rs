mod control;
mod daemon;
mod dns;
mod echo;
mod host;
mod ipc;
mod logs;
mod service;
mod settings;

use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use weft_session::StaticKeypair;

#[derive(Parser)]
#[command(name = "weftd", version, about = "Weft daemon")]
struct Args {
    #[command(flatten)]
    options: Options,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Args, Clone)]
struct Options {
    /// Directory for the device key, settings and logs [default: platform specific]
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Control socket or pipe [default: $WEFT_SOCKET or platform specific]
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// TUN interface name
    #[arg(long, global = true, default_value = weft_tun::DEFAULT_NAME)]
    tun: String,
    /// UDP port for peer traffic [default: random]
    #[arg(long, global = true)]
    port: Option<u16>,
    /// Answer pings without a TUN interface, for tests
    #[arg(long, global = true, hide = true)]
    echo: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the system service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start weftd as a system service
    Install,
    /// Stop and remove the system service
    Uninstall,
    #[cfg(windows)]
    #[command(hide = true)]
    Run,
}

fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    return std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join("Weft");
    #[cfg(target_os = "macos")]
    return PathBuf::from("/Library/Application Support/Weft");
    #[cfg(not(any(windows, target_os = "macos")))]
    return PathBuf::from("/var/lib/weft");
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = match args.command {
        Some(Command::Service { action: ServiceAction::Install }) => service::install().map_err(Into::into),
        Some(Command::Service { action: ServiceAction::Uninstall }) => service::uninstall().map_err(Into::into),
        #[cfg(windows)]
        Some(Command::Service { action: ServiceAction::Run }) => run_service(args.options),
        None => {
            init_logging(None);
            run(args.options, shutdown())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("weftd: {error}");
            tracing::error!(%error, "weftd failed");
            ExitCode::FAILURE
        }
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Punching through a symmetric NAT briefly needs several hundred sockets.
#[cfg(unix)]
fn raise_file_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    if let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) {
        let wanted = hard.min(4096);
        if soft < wanted
            && let Err(error) = setrlimit(Resource::RLIMIT_NOFILE, wanted, hard)
        {
            tracing::debug!(%error, "cannot raise the open file limit");
        }
    }
}

/// The state directory holds the device's private key, the hosted server's data and its panel
/// password, so only the daemon's own user may read it.
#[cfg(unix)]
fn protect(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        tracing::warn!(dir = %dir.display(), %error, "cannot restrict the state directory");
    }
}

/// On Windows, ProgramData lets every user read new folders: keep only SYSTEM and administrators.
#[cfg(windows)]
fn protect(dir: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if dir != default_state_dir() {
        return;
    }
    let result = std::process::Command::new("icacls")
        .arg(dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/T", "/Q"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    if !result.is_ok_and(|status| status.success()) {
        tracing::warn!(dir = %dir.display(), "cannot restrict the state directory");
    }
}

fn run(options: Options, shutdown: impl Future<Output = ()>) -> Result<(), BoxError> {
    #[cfg(unix)]
    raise_file_limit();
    let state_dir = options.state_dir.unwrap_or_else(default_state_dir);
    std::fs::create_dir_all(&state_dir)?;
    protect(&state_dir);
    let keypair = StaticKeypair::load_or_create(&state_dir.join("key"))?;
    let settings = settings::SettingsFile::new(state_dir.join("weftd.toml"));
    let socket = options.socket.unwrap_or_else(weft_ipc::socket_path);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = ipc::bind(&socket)?;
        let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(64);
        let daemon_options = daemon::Options {
            tun_name: options.tun,
            port: options.port,
            echo: options.echo,
            state_dir: state_dir.clone(),
        };
        let daemon = daemon::Daemon::new(keypair, settings, daemon_options, commands_rx).await?;
        tokio::spawn(ipc::serve(listener, commands_tx));
        tokio::select! {
            _ = daemon.run() => {}
            _ = shutdown => tracing::info!("shutting down"),
        }
        ipc::cleanup(&socket);
        Ok(())
    })
}

#[cfg(windows)]
fn run_service(options: Options) -> Result<(), BoxError> {
    let state_dir = options.state_dir.clone().unwrap_or_else(default_state_dir);
    std::fs::create_dir_all(&state_dir)?;
    protect(&state_dir);
    init_logging(Some(state_dir.join("weftd.log")));
    service::windows::dispatch(Box::new(move |mut stop| {
        let stopped = async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        };
        run(options, stopped).map_err(std::io::Error::other)
    }))?;
    Ok(())
}

fn init_logging(file: Option<PathBuf>) {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let (writer, ansi) =
        match file.and_then(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).ok()) {
            Some(file) => (BoxMakeWriter::new(std::sync::Mutex::new(file)), false),
            None => (BoxMakeWriter::new(std::io::stderr), true),
        };
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_ansi(ansi).with_writer(writer))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(logs::Recent))
        .init();
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
