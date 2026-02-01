mod client;
mod server;
mod signal;
mod storage;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use url::Url;

#[derive(Parser)]
#[command(name = "signal-mcp")]
#[command(about = "Signal MCP server and CLI")]
struct Cli {
    /// Config directory
    #[arg(long, default_value = "~/.config/signal-mcp")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server
    Server {
        /// Use stdio instead of Unix socket (for MCP clients)
        #[arg(long)]
        stdio: bool,
        /// Run as a background daemon
        #[arg(long)]
        daemon: bool,
    },
    /// Stop the daemon (if running)
    Stop,
    /// Link this device to your Signal account
    Link {
        /// Device name shown in Signal
        #[arg(short, long, default_value = "signal-mcp")]
        name: String,
    },
    /// Send a message (via server)
    Send {
        /// Recipient: phone number, name, or UUID
        recipient: String,
        /// Message text
        message: String,
    },
    /// Check server status
    Status,
    /// List contacts
    Contacts {
        /// Filter by name or phone
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// List groups
    Groups,
}

/// Expand ~ to home directory in paths.
fn expand_path(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    PathBuf::from(path)
}

/// Print a QR code for the given URL to the terminal.
fn print_qr(url: Url) {
    use qrcode::render::unicode;
    use qrcode::QrCode;

    let code = QrCode::new(url.as_str()).unwrap();
    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();

    println!("\nScan this QR code with Signal app:\n");
    println!("{}", image);
    println!("\nOr use this URL: {}\n", url);
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_dir = expand_path(&cli.config);
    std::fs::create_dir_all(&config_dir)?;

    // Handle daemon mode specially - must fork BEFORE creating tokio runtime
    if let Commands::Server { daemon: true, .. } = &cli.command {
        server::daemonize(&config_dir)?;
    }

    // Handle sync commands that don't need tokio
    match &cli.command {
        Commands::Stop => {
            return server::stop_daemon(&config_dir);
        }
        Commands::Groups => {
            println!("Groups not yet implemented.");
            return Ok(());
        }
        _ => {}
    }

    // Now create tokio runtime and run async commands
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main(cli, config_dir))
}

async fn async_main(cli: Cli, config_dir: PathBuf) -> Result<()> {
    // Init tracing after daemonize (so logs go to file in daemon mode)
    if !matches!(cli.command, Commands::Server { daemon: true, .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive(tracing::Level::WARN.into()),
            )
            .init();
    }

    match cli.command {
        Commands::Server { stdio, daemon } => {
            let server = server::Server::new(&config_dir).await?;
            if stdio {
                server.run_stdio().await?;
            } else {
                if !daemon {
                    println!("Starting server...");
                }
                server.run().await?;
            }
        }
        Commands::Stop | Commands::Groups => {
            // Handled above in sync section
            unreachable!()
        }
        Commands::Link { name } => {
            println!("Linking device as '{}'...", name);
            let store_path = config_dir.join("signal.db");
            signal::link_device(&store_path, &name, print_qr).await?;
            println!("Successfully linked!");
        }
        Commands::Send { recipient, message } => {
            let client = crate::client::Client::new(&config_dir);
            client.send_message(&recipient, &message).await?;
        }
        Commands::Status => {
            let client = crate::client::Client::new(&config_dir);
            client.status().await?;
        }
        Commands::Contacts { filter } => {
            let client = crate::client::Client::new(&config_dir);
            client.list_contacts(filter.as_deref()).await?;
        }
    }

    Ok(())
}
