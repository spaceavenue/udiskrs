mod config;
mod password;
mod property;
mod udisks;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use zbus::blocking::Connection;

use crate::udisks::Manager;

#[derive(Parser)]
#[command(name = "udiskrs", author, version, about = "UDisks2 mount manager")]
pub struct Cli {
  #[command(subcommand)]
  pub command: Commands,

  #[arg(short, long, global = true)]
  pub config: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Commands {
  /// Auto-mount hotplugged devices
  Auto,
  /// Mount a block device. Also unlocks LUKS containers
  Mount {
    /// Device name or path (e.g. /dev/sda1 or sda1)
    device: PathBuf,

    #[arg(short, long)]
    options: Option<String>,
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
  let manager = Manager::new(conn, args.config.as_deref());

  match args.command {
    Commands::Auto => manager.run_automount()?,
    Commands::Mount { device, options } => manager.mount_device(&device, options.as_deref())?,
    Commands::Unmount { target } => manager.unmount_target(&target)?,
  }
  Ok(())
}
