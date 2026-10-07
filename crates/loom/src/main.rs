use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use loom::config::Config;
use loom::db::Db;
use loom::{Server, server_link};
use weft_session::StaticKeypair;

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
    std::fs::create_dir_all(&config.data_dir)?;
    let keypair = StaticKeypair::load_or_create(&config.data_dir.join("key"))?;

    match cli.command.unwrap_or(Command::Run) {
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
