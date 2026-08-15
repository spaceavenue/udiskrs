use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use notify_rust::Notification;
use zbus::blocking::Connection;
use zbus::blocking::fdo::ObjectManagerProxy;
use zbus::fdo::ManagedObjects;
use zbus::names::OwnedInterfaceName;
use zbus::proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Str, Value};

use crate::config;

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
  fn unlock(&self, passphrase: &str, options: HashMap<&str, Value<'_>>) -> zbus::Result<String>;
  fn lock(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
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
    device: &str,
    mount_options: Option<&str>,
  ) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ObjectManagerProxy::builder(&self.conn).build()?;
    let objects = manager.get_managed_objects()?;

    let Some(path) = find_block_object(&objects, device) else {
      send_notify("Mount Failed", &format!("No such device: {device}"));
      return Ok(());
    };
    let interfaces = &objects[&path];
    let mount_options = self.decode_mount_options(interfaces, mount_options);

    if interfaces.contains_key(IFACE_FILESYSTEM) {
      match self.mount_filesystem(&path, mount_options.as_deref()) {
        Ok(mount_point) => {
          send_notify(
            "Drive Mounted",
            &format!("Mounted {device} at {mount_point}"),
          );
        }
        Err(e) => {
          send_notify("Mount Failed", &e.to_string());
          return Err(Box::new(e));
        }
      }
      return Ok(());
    }

    if interfaces.contains_key(IFACE_ENCRYPTED) {
      send_notify("Encrypted Drive", &format!("Unlocking {device}..."));
      let passphrase = prompt_password("Passphrase: ")?;

      let cleartext_path = match self.unlock_encrypted(&path, &passphrase) {
        Ok(p) => p,
        Err(e) => {
          send_notify("Unlock Failed", &e.to_string());
          return Err(Box::new(e));
        }
      };

      match self.mount_filesystem(&cleartext_path, mount_options.as_deref()) {
        Ok(mount_point) => {
          send_notify(
            "Drive Unlocked & Mounted",
            &format!("Mounted {device} at {mount_point}"),
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
      &format!("{device} has neither a filesystem nor an encrypted payload"),
    );
    Ok(())
  }

  pub fn unmount_target(&self, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ObjectManagerProxy::builder(&self.conn).build()?;
    let objects = manager.get_managed_objects()?;
    let target_is_dir = Path::new(target).is_dir();

    let hit = objects.iter().find(|(path, interfaces)| {
      let Some(fs_props) = interfaces.get(IFACE_FILESYSTEM) else {
        return false;
      };
      if target_is_dir {
        decode_bytes_array(fs_props, "MountPoints")
          .iter()
          .any(|mp| mp == target)
      } else {
        find_block_object(&objects, target).as_ref() == Some(*path)
      }
    });

    let Some((path, interfaces)) = hit else {
      send_notify("Unmount Failed", &format!("{target} is not a known mount"));
      return Ok(());
    };
    let path = path.clone();

    let fs = FilesystemProxyBlocking::builder(&self.conn)
      .path(path.as_str())?
      .build()?;

    if let Err(e) = fs.unmount(HashMap::new()) {
      send_notify("Unmount Failed", &e.to_string());
      return Err(Box::new(e));
    }
    send_notify("Drive Unmounted", &format!("Unmounted {target}"));

    if interfaces.contains_key(IFACE_ENCRYPTED) {
      let enc = EncryptedProxyBlocking::builder(&self.conn)
        .path(path.as_str())?
        .build()?;
      match enc.lock(HashMap::new()) {
        Ok(()) => send_notify("Drive Locked", &format!("Locked container for {target}")),
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
    for path in initial.keys() {
      self.handle_new_object(path, &initial);
    }

    let stream = manager.receive_interfaces_added()?;
    send_notify("Automount Active", "Listening for UDisks2 events...");

    for signal in stream {
      let Ok(args) = signal.args() else { continue };
      let objects = manager.get_managed_objects()?;
      self.handle_new_object(
        &OwnedObjectPath::from(args.object_path().to_owned()),
        &objects,
      );
    }

    Ok(())
  }

  fn unlock_encrypted(
    &self,
    path: &OwnedObjectPath,
    passphrase: &str,
  ) -> zbus::Result<OwnedObjectPath> {
    let enc = EncryptedProxyBlocking::builder(&self.conn)
      .path(path.as_str())?
      .build()?;
    let cleartext = enc.unlock(passphrase, HashMap::new())?;
    OwnedObjectPath::try_from(cleartext).map_err(|e| zbus::Error::Failure(e.to_string()))
  }

  fn mount_filesystem(
    &self,
    path: &OwnedObjectPath,
    mount_options: Option<&str>,
  ) -> zbus::Result<String> {
    let fs = FilesystemProxyBlocking::builder(&self.conn)
      .path(path.as_str())?
      .build()?;
    let mut options = HashMap::new();
    if let Some(o) = mount_options
      && !o.is_empty()
    {
      options.insert("options", Value::from(o));
    }
    fs.mount(options)
  }

  fn try_mount_with_retry(
    &self,
    path: &OwnedObjectPath,
    mount_options: Option<&str>,
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

  fn handle_new_object(&self, path: &OwnedObjectPath, objects: &ManagedObjects) {
    let Some(interfaces) = objects.get(path) else {
      return;
    };
    if !should_automount(interfaces) {
      return;
    }

    if interfaces.contains_key(IFACE_FILESYSTEM) {
      match self.try_mount_with_retry(path, None) {
        Ok(mp) => send_notify("Drive Mounted", &format!("Auto-mounted {path} at {mp}")),
        Err(e) => send_notify("Automount Failed", &format!("{path}: {e}")),
      }
    } else if interfaces.contains_key(IFACE_ENCRYPTED) {
      // unattended background listener, no terminal to prompt on, can't safely guess a
      // passphrase
      send_notify(
        "Encrypted Drive Attached",
        &format!("{path} needs to be unlocked manually!"),
      );
    }
  }

  fn decode_mount_options(
    &self,
    interfaces: &HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>,
    cli_options: Option<&str>,
  ) -> Option<String> {
    if let Some(opts) = cli_options {
      return Some(opts.to_string());
    }

    let block_props = interfaces.get(IFACE_BLOCK)?;

    if let Some(uuid) = decode_string(block_props, "IdUUID")
      && let Some(opts) = self.config.get(&uuid)
    {
      return Some(opts.clone());
    }

    if let Some(dev) = decode_bytes(block_props, "Device")
      && let Some(opts) = self.config.get(&dev)
    {
      return Some(opts.clone());
    }

    None
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

fn decode_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
  Some(
    props
      .get(key)
      .and_then(|o| Value::try_from(o).ok())
      .and_then(|v| Str::try_from(v).ok())?
      .to_string(),
  )
}

fn decode_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
  props
    .get(key)
    .and_then(|v| bool::try_from(v).ok())
    .unwrap_or(false)
}

/// `UDisks2` represents device/mount-point paths as null-terminated byte arrays (the 'ay' D-Bus
/// type), not strings, so we decode one out of a property map. `Vec<T>` only converts from an owned
/// `Value`, not `&OwnedValue` directly, so we go via the borrowed `Value<'_>` conversion first.
fn decode_bytes(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
  let owned = props.get(key)?;
  let value: Value<'_> = Value::try_from(owned).ok()?;
  let bytes: Vec<u8> = value.try_into().ok()?;
  Some(
    String::from_utf8_lossy(&bytes)
      .trim_end_matches('\0')
      .to_string(),
  )
}

/// 'aay' - an array of the above, used for the Filesystem.MountPoints property (a device can be
/// mounted at more than one place)
fn decode_bytes_array(props: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
  let Some(owned) = props.get(key) else {
    return Vec::new();
  };
  let Ok(value) = Value::try_from(owned) else {
    return Vec::new();
  };
  let Ok(entries) = <Vec<Vec<u8>>>::try_from(value) else {
    return Vec::new();
  };
  entries
    .into_iter()
    .map(|bytes| {
      String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .to_string()
    })
    .collect()
}

/// Resolve the supplied device argument to a `UDisks2` object path by asking `UDisks2` what every
/// object's real device node is.
fn find_block_object(objects: &ManagedObjects, device: &str) -> Option<OwnedObjectPath> {
  let target = if device.starts_with("/dev/") {
    device.to_string()
  } else {
    format!("/dev/{device}")
  };

  objects.iter().find_map(|(path, interfaces)| {
    let block_props = interfaces.get(IFACE_BLOCK)?;
    let device_matches = decode_bytes(block_props, "Device").as_deref() == Some(&target)
      || decode_bytes(block_props, "PreferredDevice").as_deref() == Some(&target);
    device_matches.then(|| path.clone())
  })
}

/// Should this newly-appeared object be auto-mounted? `UDisks2` exposes `HintAuto`/`HintIgnore` on
/// the Block interface precisely so udev rules, fstab entries, or the admin can opt a device out of
/// automounting
fn should_automount(interfaces: &HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>) -> bool {
  match interfaces.get(IFACE_BLOCK) {
    Some(block_props) => {
      !decode_bool(block_props, "HintIgnore") && decode_bool(block_props, "HintAuto")
    }
    None => false,
  }
}

fn prompt_password(prompt: &str) -> std::io::Result<String> {
  println!("{prompt}");
  std::io::stdout().flush()?;

  let _ = Command::new("stty").arg("-echo").status();

  let mut password = String::new();
  let res = std::io::stdin().lock().read_line(&mut password);

  let _ = Command::new("stty").arg("echo").status();
  println!();

  res?;

  if password.ends_with('\n') {
    password.pop();
    if password.ends_with('\r') {
      password.pop();
    }
  }

  Ok(password)
}
