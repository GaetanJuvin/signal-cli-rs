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
    Server,
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .init();

    let cli = Cli::parse();
    let config_dir = expand_path(&cli.config);
    std::fs::create_dir_all(&config_dir)?;

    match cli.command {
        Commands::Server => {
            println!("Starting server...");
            let server = server::Server::new(&config_dir).await?;
            server.run().await?;
        }
        Commands::Link { name } => {
            println!("Linking device as '{}'...", name);
            let store_path = config_dir.join("signal.db");
            signal::link_device(&store_path, &name, print_qr).await?;
            println!("Successfully linked!");
        }
        Commands::Send { recipient, message } => {
            println!("Sending to {}: {}", recipient, message);
            // TODO: send via server
        }
        Commands::Status => {
            println!("Checking status...");
            // TODO: check server status
        }
    }

    Ok(())
}
