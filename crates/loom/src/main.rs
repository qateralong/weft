use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use loom::config::Config;
use loom::db::Db;
use loom::{Server, server_link};
use weft_session::StaticKeypair;

#[cfg(unix)]
mod admin_cli;

#[derive(Parser)]
#[command(name = "loom", version, about = "Weft coordination and relay server")]
struct Cli {
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (default)
    Run,
    /// Print the server link to share with users
    Link {
        /// Public host name or IP address of this server
        #[arg(long)]
        host: Option<String>,
    },
    /// Manage the running server
    Admin {
        #[command(subcommand)]
        action: AdminCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Show devices, networks and relay traffic
    Stats,
    /// List networks
    Networks,
    /// List devices
    Devices {
        /// Only online devices
        #[arg(long)]
        online: bool,
        /// Only blocked devices
        #[arg(long)]
        blocked: bool,
    },
    /// Disconnect a device and refuse it from now on
    Block {
        /// Nickname, address or key
        device: String,
    },
    /// Allow a blocked device again
    Unblock {
        /// Nickname, address or key
        device: String,
    },
    /// Delete a network for all its members
    DeleteNetwork {
        name: String,
        /// Confirm the deletion
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("loom: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = match &cli.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    #[cfg(unix)]
    let admin_socket = config.data_dir.join("admin.sock");
    let command = cli.command.unwrap_or(Command::Run);
    if let Command::Admin { action } = command {
        #[cfg(unix)]
        return admin_cli::run(&admin_socket, action);
        #[cfg(not(unix))]
        return Err(format!("loom admin needs a Unix socket and is not available here ({action:?})").into());
    }
    std::fs::create_dir_all(&config.data_dir)?;
    let keypair = StaticKeypair::load_or_create(&config.data_dir.join("key"))?;

    match command {
        Command::Link { host } => {
            let host = host.or(config.public_host.clone()).ok_or("set public_host in the config or pass --host")?;
            println!("{}", server_link(&host, config.listen.port(), &keypair.public())?);
            Ok(())
        }
        Command::Run => {
            let db = Db::open(&config.data_dir.join("loom.db"))?;
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let server = Server::start(config.clone(), keypair, db).await?;
                #[cfg(unix)]
                let server = {
                    let mut server = server;
                    server.serve_admin(&admin_socket)?;
                    server
                };
                tracing::info!(tcp = %server.tcp_addr, udp = %server.udp_addr, key = %server.public_key, "loom started");
                if let Some(host) = &config.public_host {
                    tracing::info!(link = %server.link(host)?, "share this link");
                }
                tokio::select! {
                    _ = server.run() => {}
                    _ = shutdown() => tracing::info!("shutting down"),
                }
                Ok::<_, Box<dyn std::error::Error>>(())
            })
        }
        Command::Admin { .. } => unreachable!("handled above"),
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
