use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify_rust::Notification;
use zbus::blocking::Connection;
use zbus::blocking::fdo::ObjectManagerProxy;
use zbus::fdo::ManagedObjects;
use zbus::proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

use crate::property::{InterfaceMap, PropertyMap};
use crate::{config, password};

const IFACE_FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const IFACE_ENCRYPTED: &str = "org.freedesktop.UDisks2.Encrypted";
const IFACE_BLOCK: &str = "org.freedesktop.UDisks2.Block";

#[proxy(
  interface = "org.freedesktop.UDisks2.Filesystem",
  default_service = "org.freedesktop.UDisks2"
)]
trait Filesystem {
  fn mount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<String>;
  fn unmount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

#[proxy(
  interface = "org.freedesktop.UDisks2.Encrypted",
  default_service = "org.freedesktop.UDisks2"
)]
trait Encrypted {
  fn unlock(
    &self,
    passphrase: &str,
    options: HashMap<&str, Value<'_>>,
  ) -> zbus::Result<OwnedObjectPath>;
  fn lock(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

#[derive(Debug, Default)]
struct DeviceHints {
  auto: bool,
  ignore: bool,
}

#[derive(Debug)]
struct FilesystemInfo {
  mount_points: Vec<PathBuf>,
}

#[derive(Debug, Default)]
struct DeviceInfo {
  filesystem: Option<FilesystemInfo>,
  is_block: bool,
  is_encrypted: bool,
  hints: DeviceHints,
  uuid: Option<String>,
  device: Option<PathBuf>,
  preferred_device: Option<PathBuf>,
  crypto_backing_device: Option<OwnedObjectPath>,
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

  fn should_automount(&self) -> bool {
    self.is_block && !self.hints.ignore && self.hints.auto
  }
}

pub struct Manager {
  conn: Connection,
  config: HashMap<String, String>,
}
impl Manager {
  pub fn new(conn: Connection, config_path: Option<&Path>) -> Self {
    let config = config::load_config(config_path);
    Self { conn, config }
  }

  pub fn mount_device(
    &self,
    device: &Path,
    mount_options: Option<&str>,
  ) -> Result<(), Box<dyn std::error::Error>> {
    let device_display = device.display();
    let manager = ObjectManagerProxy::builder(&self.conn).build()?;
    let objects = manager.get_managed_objects()?;

    let Some((path, info)) = find_block_device(&objects, device) else {
      send_notify("Mount Failed", &format!("No such device: {device_display}"));
      return Ok(());
    };

    let mount_options =
      self.decode_mount_options(info.uuid.as_deref(), info.device.as_deref(), mount_options);

    if info.filesystem.is_some() {
      match self.try_mount_with_retry(&path, &mount_options) {
        Ok(mount_point) => {
          send_notify(
            "Drive Mounted",
            &format!("Mounted {device_display} at {mount_point}"),
          );
        }
        Err(e) => {
          send_notify("Mount Failed", &e.to_string());
          return Err(Box::new(e));
        }
      }
      return Ok(());
    }

    if info.is_encrypted {
      send_notify("Encrypted Drive", &format!("Unlocking {device_display}..."));
      let passphrase = password::prompt_password("Passphrase: ")?;

      let cleartext_path = match self.unlock_encrypted(&path, &passphrase) {
        Ok(p) => p,
        Err(e) => {
          send_notify("Unlock Failed", &e.to_string());
          return Err(Box::new(e));
        }
      };

      match self.try_mount_with_retry(&cleartext_path, &mount_options) {
        Ok(mount_point) => {
          send_notify(
            "Drive Unlocked & Mounted",
            &format!("Mounted {device_display} at {mount_point}"),
          );
        }
        Err(e) => {
          send_notify("Mount Failed", &e.to_string());
          return Err(Box::new(e));
        }
      }
      return Ok(());
    }

    send_notify(
      "Mount Failed",
      &format!("{device_display} has neither a filesystem nor an encrypted payload"),
    );
    Ok(())
  }

  pub fn unmount_target(&self, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ObjectManagerProxy::builder(&self.conn).build()?;
    let objects = manager.get_managed_objects()?;
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
      send_notify(
        "Unmount Failed",
        &format!("{target} is not a known mount or device"),
      );
      return Ok(());
    };

    if info.filesystem.is_some() {
      let fs = FilesystemProxyBlocking::builder(&self.conn)
        .path(path.as_str())?
        .build()?;

      if let Err(e) = fs.unmount(HashMap::new()) {
        send_notify("Unmount Failed", &e.to_string());
        return Err(Box::new(e));
      }
      send_notify("Drive Unmounted", &format!("Unmounted {target}"));
    }

    // Lock LUKS container:
    // If the unmounted device is a cleartext child, its CryptoBackingDevice property points to the
    // encrypted container
    if let Some(ref backing_path) = info.crypto_backing_device {
      let enc = EncryptedProxyBlocking::builder(&self.conn)
        .path(backing_path.as_str())?
        .build()?;
      match enc.lock(HashMap::new()) {
        Ok(()) => send_notify(
          "Drive Locked",
          &format!("Locked encrypted container for {target}"),
        ),
        Err(e) => send_notify("Lock Failed", &e.to_string()),
      }
    } else if info.is_encrypted {
      let enc = EncryptedProxyBlocking::builder(&self.conn)
        .path(path.as_str())?
        .build()?;
      match enc.lock(HashMap::new()) {
        Ok(()) => send_notify("Drive Locked", &format!("Locked container {target}")),
        Err(e) => send_notify("Lock Failed", &e.to_string()),
      }
    }

    Ok(())
  }

  pub fn run_automount(&self) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ObjectManagerProxy::builder(&self.conn).build()?;

    // devices already attached before we started listening won't generate an InterfacesAdded
    // signal, so sweep once at startup too
    let initial = manager.get_managed_objects()?;
    for (path, ifaces) in &initial {
      let info = DeviceInfo::from_interfaces(ifaces);
      self.handle_device_event(&path.as_ref(), &info);
    }

    let stream = manager.receive_interfaces_added()?;
    send_notify("Automount Active", "Listening for UDisks2 events...");

    for signal in stream {
      let Ok(args) = signal.args() else { continue };
      let info = DeviceInfo::from_interfaces(args.interfaces_and_properties());
      self.handle_device_event(args.object_path(), &info);
    }

    Ok(())
  }

  fn handle_device_event(&self, path: &ObjectPath<'_>, dev: &DeviceInfo) {
    if !dev.should_automount() {
      return;
    }

    if dev.is_encrypted {
      send_notify(
        "Encrypted Drive Attached",
        &format!("{path} needs to be unlocked manually!"),
      );
      return;
    }

    if dev.filesystem.is_some() {
      let mount_options =
        self.decode_mount_options(dev.uuid.as_deref(), dev.device.as_deref(), None);
      let owned_path = OwnedObjectPath::from(path.to_owned());
      match self.try_mount_with_retry(&owned_path, &mount_options) {
        Ok(mp) => send_notify("Drive Mounted", &format!("Auto-mounted {path} at {mp}")),
        Err(e) => send_notify("Automount Failed", &format!("{path}: {e}")),
      }
    }
  }

  fn unlock_encrypted(
    &self,
    path: &OwnedObjectPath,
    passphrase: &str,
  ) -> zbus::Result<OwnedObjectPath> {
    let enc = EncryptedProxyBlocking::builder(&self.conn)
      .path(path.as_str())?
      .build()?;
    enc.unlock(passphrase, HashMap::new())
  }

  fn mount_filesystem(
    &self,
    path: &OwnedObjectPath,
    mount_options: &HashMap<&str, Value<'_>>,
  ) -> zbus::Result<String> {
    let fs = FilesystemProxyBlocking::builder(&self.conn)
      .path(path.as_str())?
      .build()?;
    fs.mount(mount_options.clone())
  }

  fn try_mount_with_retry(
    &self,
    path: &OwnedObjectPath,
    mount_options: &HashMap<&str, Value<'_>>,
  ) -> zbus::Result<String> {
    // UDisks2 needs a moment to finish probing a just-added filesystem before Mount succeeds,
    // so we retry briefly
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
      match self.mount_filesystem(path, mount_options) {
        Ok(mp) => return Ok(mp),
        Err(_) if Instant::now() < deadline => {
          std::thread::sleep(Duration::from_millis(200));
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

fn send_notify(title: &str, body: &str) {
  let _ = Notification::new()
    .summary(title)
    .body(body)
    .appname("udiskrs")
    .show();
  println!("[{title}] {body}");
}

fn find_block_device(
  objects: &ManagedObjects,
  device: &Path,
) -> Option<(OwnedObjectPath, DeviceInfo)> {
  let target_buf;
  let target: &Path = if device.starts_with("/dev") {
    device
  } else {
    target_buf = Path::new("/dev").join(device);
    &target_buf
  };

  objects.iter().find_map(|(path, ifaces)| {
    let info = DeviceInfo::from_interfaces(ifaces);
    let matches =
      info.device.as_deref() == Some(target) || info.preferred_device.as_deref() == Some(target);
    matches.then(|| (path.clone(), info))
  })
}
