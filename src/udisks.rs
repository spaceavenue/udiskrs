use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify_rust::Notification;
use zbus::blocking::Connection;
use zbus::blocking::fdo::ObjectManagerProxy;
use zbus::fdo::ManagedObjects;
use zbus::names::OwnedInterfaceName;
use zbus::proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::property::{InterfaceMap, PropertyMap};
use crate::{config, password};

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
  fn is_mounted(&self) -> bool {
    self
      .filesystem
      .as_ref()
      .is_some_and(|fs| !fs.mount_points.is_empty())
  }

  fn should_automount(&self) -> bool {
    self.is_block && !self.hints.ignore && self.hints.auto && !self.is_mounted()
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

  fn object_manager(&self) -> zbus::Result<ObjectManagerProxy<'_>> {
    ObjectManagerProxy::builder(&self.conn)
      .destination(UDISKS2_SERVICE)?
      .path(UDISKS2_PATH)?
      .build()
  }

  pub fn mount_device(
    &self,
    device: &Path,
    mount_options: Option<&str>,
  ) -> Result<(), Box<dyn std::error::Error>> {
    let device_display = device.display();
    let manager = self.object_manager()?;
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
    let manager = self.object_manager()?;
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
    let manager = self.object_manager()?;

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

  pub fn info(&self, target: &str, all: bool) -> Result<(), Box<dyn std::error::Error>> {
    let manager = self.object_manager()?;
    let objects = manager.get_managed_objects()?;
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
      send_notify(
        "Failed to get info",
        &format!("{target} is not a known mount or device"),
      );
      return Ok(());
    };
    let info = DeviceInfo::from_interfaces(ifaces);
    let mount_options =
      self.decode_mount_options(info.uuid.as_deref(), info.device.as_deref(), None);
    print_device_info(&info, &mount_options);
    if all {
      print_udisks_properties(&path.as_ref(), ifaces);
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

fn print_device_info(info: &DeviceInfo, mount_options: &HashMap<&str, Value<'_>>) {
  let device = info
    .device
    .as_deref()
    .map_or("-", |p| p.to_str().unwrap_or("-"));
  let preferred_device = info
    .preferred_device
    .as_deref()
    .map_or("-", |p| p.to_str().unwrap_or("-"));
  let backing_dev = info
    .crypto_backing_device
    .as_deref()
    .map_or("-", |p| p.as_str());
  let uuid = info.uuid.as_deref().unwrap_or("-");
  let is_block = info.is_block;
  let is_encrypted = info.is_encrypted;
  let mp_or_no_fs = match &info.filesystem {
    Some(fs) => {
      let mount_points = fs
        .mount_points
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
      format!("Mount Point(s):     {mount_points}")
    }
    None => format!("Filesystem:         -"),
  };
  let auto = info.hints.auto;
  let ignore = info.hints.ignore;
  let should_automount = info.should_automount();
  let config_opts = mount_options
    .get("options")
    .and_then(|v| <&str>::try_from(v).ok())
    .unwrap_or("-");
  let output = format!(
    r"
Device:             {device}
Preferred Device:   {preferred_device}
UUID:               {uuid}
Is Block Device:    {is_block}
Is Encrypted:       {is_encrypted}
Crypto Backing Dev: {backing_dev}
{mp_or_no_fs}
Hint Auto:          {auto}
Hint Ignore:        {ignore}
Should Automount:   {should_automount}
Config Mount Opts:  {config_opts}"
  );
  println!("Device Summary");
  print!("------------------------------------------");
  println!("{output}");
}

fn print_udisks_properties(
  path: &ObjectPath<'_>,
  ifaces: &HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>,
) {
  println!("\nUDisks2 D-Bus Properties ({path})");
  println!("------------------------------------------");

  let mut sorted_ifaces: Vec<_> = ifaces.iter().collect();
  sorted_ifaces.sort_by_key(|(name, _)| name.as_str());

  for (iface, props) in sorted_ifaces {
    println!("\n[{iface}]");
    let mut sorted_props: Vec<_> = props.iter().collect();
    sorted_props.sort_by_key(|(k, _)| k.as_str());

    for (k, v) in sorted_props {
      let formatted = format_prop_value(v);
      println!("  {k:<24} {formatted}");
    }
  }
}

fn format_prop_value(val: &OwnedValue) -> String {
  let val_ref: &Value<'_> = val;

  // Pretty-print byte arrays ('ay') as strings if valid UTF-8
  if let Ok(bytes) = <Vec<u8>>::try_from(val_ref.clone()) {
    let lossy = String::from_utf8_lossy(&bytes);
    let trimmed = lossy.trim_end_matches('\0');
    if !trimmed.is_empty()
      && trimmed
        .chars()
        .all(|c| !c.is_control() || c == '\n' || c == '\t')
    {
      return format!("\"{trimmed}\"");
    }
  }

  // Pretty-print arrays of byte arrays ('aay') such as MountPoints/Symlinks
  if let Ok(entries) = <Vec<Vec<u8>>>::try_from(val_ref.clone()) {
    let items: Vec<String> = entries
      .into_iter()
      .map(|b| {
        let lossy = String::from_utf8_lossy(&b);
        format!("\"{}\"", lossy.trim_end_matches('\0'))
      })
      .collect();
    return format!("[{}]", items.join(", "));
  }

  format!("{val_ref}")
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
