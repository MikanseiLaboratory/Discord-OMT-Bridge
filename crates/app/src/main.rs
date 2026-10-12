use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use discord_omt_bridge::{run, Config};

#[derive(Debug, Parser)]
#[command(name = "discord-omt-bridge")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Join a voice channel and publish per-user audio, camera, and Go Live.
    Run {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Publish a color bar and tone without connecting to Discord.
    Colorbar {
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long, default_value_t = true)]
        omt: bool,
        #[arg(long, default_value_t = false)]
        ndi: bool,
    },
}

fn main() -> anyhow::Result<()> {
    if discord_omt_bridge::maybe_run_av1_worker() {
        return Ok(());
    }
    cli_main()
}

#[tokio::main]
async fn cli_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match Cli::parse().command {
        Command::Colorbar { seconds, omt, ndi } => {
            egress::run_colorbar("Discord Colorbar", omt, ndi, Duration::from_secs(seconds))?;
        }
        Command::Run { config } => {
            let config = Config::load(&config)?;
            run(config).await?;
        }
    }
    Ok(())
}
