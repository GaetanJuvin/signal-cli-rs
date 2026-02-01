mod signal;
mod storage;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "signal-mcp")]
#[command(about = "Signal MCP server and CLI")]
struct Cli {
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

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server => println!("Starting server..."),
        Commands::Link { name } => println!("Linking as '{}'...", name),
        Commands::Send { recipient, message } => {
            println!("Sending to {}: {}", recipient, message)
        }
        Commands::Status => println!("Checking status..."),
    }
}
