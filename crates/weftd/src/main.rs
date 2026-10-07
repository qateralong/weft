#[cfg(unix)]
mod control;
#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod ipc;
#[cfg(unix)]
mod settings;

use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    use std::path::PathBuf;

    use clap::Parser;
    use weft_session::StaticKeypair;

    #[derive(Parser)]
    #[command(name = "weftd", version, about = "Weft daemon")]
    struct Args {
        /// Directory for the device key and settings
        #[arg(long, default_value = "/var/lib/weft")]
        state_dir: PathBuf,
        /// Control socket path [default: $WEFT_SOCKET or /run/weft/weftd.sock]
        #[arg(long)]
        socket: Option<PathBuf>,
        /// TUN interface name
        #[arg(long, default_value = weft_tun::DEFAULT_NAME)]
        tun: String,
        /// UDP port for peer traffic [default: random]
        #[arg(long)]
        port: Option<u16>,
    }

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(&args.state_dir)?;
        let keypair = StaticKeypair::load_or_create(&args.state_dir.join("key"))?;
        let settings = settings::SettingsFile::new(args.state_dir.join("weftd.toml"));
        let socket = args.socket.unwrap_or_else(weft_ipc::socket_path);
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let listener = ipc::bind(&socket)?;
            let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(64);
            let daemon = daemon::Daemon::new(keypair, settings, args.tun, args.port, commands_rx).await?;
            tokio::spawn(ipc::serve(listener, commands_tx));
            tokio::select! {
                _ = daemon.run() => {}
                _ = shutdown() => tracing::info!("shutting down"),
            }
            let _ = std::fs::remove_file(&socket);
            Ok(())
        })
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("weftd: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("weftd: this platform is not supported yet");
    ExitCode::FAILURE
}
