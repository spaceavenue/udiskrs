use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_executor;
use futures_lite::StreamExt;
use zbus::fdo::ObjectManagerProxy;
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, proxy};

use crate::property::{InterfaceMap, PropertyMap};
use crate::{config, password, util};

const UDISKS2_SERVICE: &str = "org.freedesktop.UDisks2";
const UDISKS2_PATH: &str = "/org/freedesktop/UDisks2";
const IFACE_FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const IFACE_ENCRYPTED: &str = "org.freedesktop.UDisks2.Encrypted";
const IFACE_BLOCK: &str = "org.freedesktop.UDisks2.Block";

#[proxy(
  interface = "org.freedesktop.UDisks2.Filesystem",
  default_service = "org.freedesktop.UDisks2"
)]
trait Filesystem {
  async fn mount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<String>;
  async fn unmount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

#[proxy(
  interface = "org.freedesktop.UDisks2.Encrypted",
  default_service = "org.freedesktop.UDisks2"
)]
trait Encrypted {
  async fn unlock(
    &self,
    passphrase: &str,
    options: HashMap<&str, Value<'_>>,
  ) -> zbus::Result<OwnedObjectPath>;
  async fn lock(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

#[derive(Debug, Default)]
pub struct DeviceHints {
  pub auto: bool,
  pub ignore: bool,
}

#[derive(Debug)]
pub struct FilesystemInfo {
  pub mount_points: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct DeviceInfo {
  pub filesystem: Option<FilesystemInfo>,
  pub is_block: bool,
  pub is_encrypted: bool,
  pub hints: DeviceHints,
  pub uuid: Option<String>,
  pub device: Option<PathBuf>,
  pub preferred_device: Option<PathBuf>,
  pub crypto_backing_device: Option<OwnedObjectPath>,
}
impl DeviceInfo {
  pub fn from_interfaces(ifaces: &impl InterfaceMap) -> Self {
    let mut info = Self {
      is_encrypted: ifaces.has_iface(IFACE_ENCRYPTED),
      ..Default::default()
    };

    if let Some(props) = ifaces.get_iface(IFACE_FILESYSTEM) {
      info.filesystem = Some(FilesystemInfo {
        mount_points: props.get_paths("MountPoints"),
      });
    }

    if let Some(props) = ifaces.get_iface(IFACE_BLOCK) {
      info.is_block = true;
      info.hints = DeviceHints {
        auto: props.get_bool("HintAuto"),
        ignore: props.get_bool("HintIgnore"),
      };
      info.uuid = props.get_str("IdUUID");
      info.device = props.get_path("Device");
      info.preferred_device = props.get_path("PreferredDevice");
      info.crypto_backing_device = props
        .get_object_path("CryptoBackingDevice")
        .filter(|p| p.as_str() != "/");
    }

    info
  }

  fn is_mounted(&self) -> bool {
    self
      .filesystem
      .as_ref()
      .is_some_and(|fs| !fs.mount_points.is_empty())
  }

  pub fn should_automount(&self) -> bool {
    self.is_block && !self.hints.ignore && self.hints.auto && !self.is_mounted()
  }
}

#[derive(Clone)]
pub struct Manager {
  conn: Connection,
  config: HashMap<String, String>,
}
impl Manager {
  pub fn new(conn: Connection, config_path: Option<&Path>) -> Self {
    let config = config::load_config(config_path);
    Self { conn, config }
  }

  async fn object_manager(&self) -> Result<ObjectManagerProxy<'_>> {
    ObjectManagerProxy::builder(&self.conn)
      .destination(UDISKS2_SERVICE)
      .context("Invalid UDisks2 destination")?
      .path(UDISKS2_PATH)
      .context("Invalid UDisks2 path")?
      .build()
      .await
      .context("Failed to connect to UDisks2 ObjectManager")
  }
  async fn get_objects(
    &self,
    manager: &ObjectManagerProxy<'_>,
  ) -> Result<HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>>>
  {
    manager
      .get_managed_objects()
      .await
      .context("Failed to retrieve managed objects from UDisks2")
  }
  pub async fn mount_device(&self, device: &Path, mount_options: Option<&str>) -> Result<()> {
    let device_display = device.display();
    let manager = self.object_manager().await?;
    let objects = self.get_objects(&manager).await?;

    let Some((path, info)) = util::find_block_device(&objects, device) else {
      return util::bail_notify("Mount Failed", format!("No such device: {device_display}"));
    };

    let mount_options =
      self.decode_mount_options(info.uuid.as_deref(), info.device.as_deref(), mount_options);

    if info.filesystem.is_some() {
      match self.try_mount_with_retry(&path, &mount_options).await {
        Ok(mount_point) => {
          util::send_notify(
            "Drive Mounted",
            format!("Mounted {device_display} at {mount_point}"),
          );
          return Ok(());
        }
        Err(e) => {
          return util::bail_notify(
            "Mount Failed",
            format!("Failed to mount {device_display}: {e}"),
          );
        }
      }
    }

    if info.is_encrypted {
      util::send_notify("Encrypted Drive", format!("Unlocking {device_display}..."));
      let passphrase = password::prompt_password("Passphrase: ")
        .context("Failed to read passphrase from terminal")?;

      let cleartext_path = match self.unlock_encrypted(&path, &passphrase).await {
        Ok(p) => p,
        Err(e) => {
          return util::bail_notify(
            "Unlock Failed",
            format!("Failed to unlock {device_display}: {e}"),
          );
        }
      };

      match self
        .try_mount_with_retry(&cleartext_path, &mount_options)
        .await
      {
        Ok(mount_point) => {
          util::send_notify(
            "Drive Unlocked & Mounted",
            format!("Mounted {device_display} at {mount_point}"),
          );
          return Ok(());
        }
        Err(e) => {
          return util::bail_notify(
            "Mount Failed",
            format!("Failed to mount unlocked device {device_display}: {e}"),
          );
        }
      }
    }

    util::bail_notify(
      "Mount Failed",
      format!("{device_display} has neither a filesystem nor an encrypted payload"),
    )
  }

  pub async fn unmount_target(&self, target: &str) -> Result<()> {
    let manager = self.object_manager().await?;
    let objects = self.get_objects(&manager).await?;

    let target_path = Path::new(target);
    let target_is_dir = target_path.is_dir();
    let target_dev = if target.starts_with("/dev/") {
      PathBuf::from(target)
    } else {
      PathBuf::from(format!("/dev/{target}"))
    };

    let hit = objects.iter().find_map(|(path, ifaces)| {
      let info = DeviceInfo::from_interfaces(ifaces);
      let matches = if target_is_dir {
        info
          .filesystem
          .as_ref()
          .is_some_and(|fs| fs.mount_points.iter().any(|mp| mp == target_path))
      } else {
        info.device.as_ref() == Some(&target_dev)
          || info.preferred_device.as_ref() == Some(&target_dev)
      };
      matches.then_some((path.clone(), info))
    });

    let Some((path, info)) = hit else {
      return util::bail_notify(
        "Unmount Failed",
        format!("{target} is not a known mount or device"),
      );
    };

    if info.filesystem.is_some() {
      let fs = FilesystemProxy::builder(&self.conn)
        .path(path.as_str())
        .context("Invalid filesystem object path")?
        .build()
        .await
        .context("Failed to create Filesystem proxy")?;

      if let Err(e) = fs.unmount(HashMap::new()).await {
        return util::bail_notify("Unmount Failed", format!("Failed to unmount {target}: {e}"));
      }
      util::send_notify("Drive Unmounted", format!("Unmounted {target}"));
    }

    // Lock LUKS container:
    // If the unmounted device is a cleartext child, its CryptoBackingDevice property points to the
    // encrypted container
    if let Some(ref backing_path) = info.crypto_backing_device {
      let enc = EncryptedProxy::builder(&self.conn)
        .path(backing_path.as_str())
        .context("Invalid encrypted container object path")?
        .build()
        .await
        .context("Failed to create Encrypted proxy for container")?;
      match enc.lock(HashMap::new()).await {
        Ok(()) => util::send_notify(
          "Drive Locked",
          format!("Locked encrypted container for {target}"),
        ),
        Err(e) => {
          return util::bail_notify(
            "Lock Failed",
            format!("Failed to lock encrypted container for {target}: {e}"),
          );
        }
      }
    } else if info.is_encrypted {
      let enc = EncryptedProxy::builder(&self.conn)
        .path(path.as_str())
        .context("Invalid encrypted object path")?
        .build()
        .await
        .context("Failed to create Encrypted proxy")?;
      match enc.lock(HashMap::new()).await {
        Ok(()) => util::send_notify("Drive Locked", format!("Locked container {target}")),
        Err(e) => {
          return util::bail_notify(
            "Lock Failed",
            format!("Failed to lock container {target}: {e}"),
          );
        }
      }
    }

    Ok(())
  }

  pub async fn run_automount(&self, ex: &async_executor::LocalExecutor<'_>) -> Result<()> {
    let manager = self.object_manager().await?;

    // devices already attached before we started listening won't generate an InterfacesAdded
    // signal, so sweep once at startup too
    let initial = self.get_objects(&manager).await?;

    for (path, ifaces) in &initial {
      let info = DeviceInfo::from_interfaces(ifaces);
      let manager = self.clone();
      let path_owned = path.clone();
      ex.spawn(async move {
        manager
          .handle_device_event(&path_owned.as_ref(), &info)
          .await;
      })
      .detach();
    }

    let mut stream = manager
      .receive_interfaces_added()
      .await
      .context("Failed to subscribe to InterfacesAdded D-Bus signals")?;
    util::send_notify("Automount Active", "Listening for UDisks2 events...");

    while let Some(signal) = stream.next().await {
      let Ok(args) = signal.args() else { continue };
      let info = DeviceInfo::from_interfaces(args.interfaces_and_properties());
      let manager = self.clone();
      let path_owned = args.object_path().to_owned();
      ex.spawn(async move {
        manager.handle_device_event(&path_owned, &info).await;
      })
      .detach();
    }

    Ok(())
  }

  pub async fn info(&self, target: &str, all: bool) -> Result<()> {
    let manager = self.object_manager().await?;
    let objects = self.get_objects(&manager).await?;

    let target_path = Path::new(target);
    let target_is_dir = target_path.is_dir();
    let target_dev = if target.starts_with("/dev/") {
      PathBuf::from(target)
    } else {
      PathBuf::from(format!("/dev/{target}"))
    };

    let hit = objects.iter().find(|(_, ifaces)| {
      let info = DeviceInfo::from_interfaces(*ifaces);
      if target_is_dir {
        info
          .filesystem
          .as_ref()
          .is_some_and(|fs| fs.mount_points.iter().any(|mp| mp == target_path))
      } else {
        info.device.as_ref() == Some(&target_dev)
          || info.preferred_device.as_ref() == Some(&target_dev)
      }
    });

    let Some((path, ifaces)) = hit else {
      return util::bail_notify(
        "Failed to get info",
        format!("{target} is not a known mount or device"),
      );
    };
    let info = DeviceInfo::from_interfaces(ifaces);
    let mount_options =
      self.decode_mount_options(info.uuid.as_deref(), info.device.as_deref(), None);
    util::print_device_info(&info, &mount_options);
    if all {
      util::print_udisks_properties(&path.as_ref(), ifaces);
    }

    Ok(())
  }

  async fn handle_device_event(&self, path: &ObjectPath<'_>, dev: &DeviceInfo) {
    if !dev.should_automount() {
      return;
    }

    if dev.is_encrypted {
      util::send_notify(
        "Encrypted Drive Attached",
        format!("{path} needs to be unlocked manually!"),
      );
      return;
    }

    if dev.filesystem.is_some() {
      let mount_options =
        self.decode_mount_options(dev.uuid.as_deref(), dev.device.as_deref(), None);
      let owned_path = OwnedObjectPath::from(path.to_owned());
      match self.try_mount_with_retry(&owned_path, &mount_options).await {
        Ok(mp) => util::send_notify("Drive Mounted", format!("Auto-mounted {path} at {mp}")),
        Err(e) => util::send_notify("Automount Failed", format!("{path}: {e:#}")),
      }
    }
  }

  async fn unlock_encrypted(
    &self,
    path: &OwnedObjectPath,
    passphrase: &str,
  ) -> Result<OwnedObjectPath> {
    let enc = EncryptedProxy::builder(&self.conn)
      .path(path.as_str())
      .context("Invalid encrypted object path")?
      .build()
      .await
      .context("Failed to build Encrypted proxy")?;
    enc
      .unlock(passphrase, HashMap::new())
      .await
      .context("Passphrase rejected or unlock operation failed")
  }

  async fn mount_filesystem(
    &self,
    path: &OwnedObjectPath,
    mount_options: &HashMap<&str, Value<'_>>,
  ) -> Result<String> {
    let fs = FilesystemProxy::builder(&self.conn)
      .path(path.as_str())
      .context("Invalid filesystem object path")?
      .build()
      .await
      .context("Failed to build Filesystem proxy")?;
    fs.mount(mount_options.clone())
      .await
      .context("UDisks2 mount method failed")
  }

  async fn try_mount_with_retry(
    &self,
    path: &OwnedObjectPath,
    mount_options: &HashMap<&str, Value<'_>>,
  ) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut delay = Duration::from_millis(10);
    loop {
      match self.mount_filesystem(path, mount_options).await {
        Ok(mp) => return Ok(mp),
        Err(_) if Instant::now() < deadline => {
          async_io::Timer::after(delay).await;
          delay = (delay * 2).min(Duration::from_millis(100));
        }
        Err(e) => return Err(e),
      }
    }
  }

  fn decode_mount_options(
    &self,
    uuid: Option<&str>,
    device: Option<&Path>,
    cli_options: Option<&str>,
  ) -> HashMap<&str, Value<'_>> {
    cli_options
      .map(String::from)
      // get options from uuid
      .or_else(|| uuid.and_then(|u| self.config.get(u).cloned()))
      // or get them from the device path
      .or_else(|| {
        let d = device?;
        let full_path = d.to_str().and_then(|s| self.config.get(s));
        let file_name = d
          .file_name()
          .and_then(|n| n.to_str())
          .and_then(|s| self.config.get(s));
        full_path.or(file_name).cloned()
      })
      // convert to a hash map
      .filter(|s| !s.is_empty())
      .map(|s| HashMap::from([("options", Value::from(s))]))
      .unwrap_or_default()
  }
}
