mod udisks;

use clap::{Parser, Subcommand};
use zbus::blocking::Connection;

#[derive(Parser)]
#[command(name = "udiskrs", author, version, about = "UDisks2 mount manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Auto-mount hotplugged devices
    Auto,
    /// Mount a block device. Also unlocks LUKS containers
    Mount {
        /// Device name or path (e.g. /dev/sda1 or sda1)
        device: String,
    },
    /// Unmount a filesystem. Also locks LUKS containers
    Unmount {
        /// Device path or mount directory
        target: String,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();
    let conn = Connection::system()?;

    match args.command {
        Commands::Auto => udisks::run_automount(&conn)?,
        Commands::Mount { device } => udisks::mount_device(&conn, &device)?,
        Commands::Unmount { target } => udisks::unmount_target(&conn, &target)?,
    }
    Ok(())
}
