mod config;
mod password;
mod property;
mod udisks;
mod util;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use zbus::Connection;

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
  /// Mount a block device. Also unlocks LUKS containers.
  Mount {
    /// Device name or path (e.g. /dev/sda1 or sda1)
    device: PathBuf,
    /// Mount options for the specified device.
    #[arg(short, long)]
    options: Option<String>,
  },
  /// Unmount a filesystem. Also locks LUKS containers.
  Unmount {
    /// Device path or mount directory
    target: String,
  },
  /// Get information about a filesystem.
  Info {
    /// Device path or mount directory
    target: String,
    /// Also print all Udisks2 properties.
    #[arg(short, long, default_value = "false")]
    all: bool,
  },
}

fn main() -> anyhow::Result<()> {
  let args = Cli::parse();
  let ex = async_executor::LocalExecutor::new();

  async_io::block_on(ex.run(async {
    let conn = Connection::system()
      .await
      .context("Failed to establish system D-Bus connection")?;
    let manager = Manager::new(conn, args.config.as_deref());

    match args.command {
      Commands::Auto => manager.run_automount(&ex).await?,
      Commands::Mount { device, options } => {
        manager.mount_device(&device, options.as_deref()).await?;
      }
      Commands::Unmount { target } => manager.unmount_target(&target).await?,
      Commands::Info { target, all } => manager.info(&target, all).await?,
    }
    Ok(())
  }))
}
