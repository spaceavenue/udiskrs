use std::collections::HashMap;
use std::path::Path;

use notify_rust::Notification;
use zbus::fdo::ManagedObjects;
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::udisks::DeviceInfo;

fn notify(title: &str, body: impl AsRef<str>) {
  let body = body.as_ref();
  let _ = Notification::new()
    .summary(title)
    .body(body)
    .appname("udiskrs")
    .show();
}

/// Send a notification and log to stdout.
pub fn log_notify(title: impl AsRef<str>, body: impl AsRef<str>) {
  let title = title.as_ref();
  let body = body.as_ref();
  notify(title, body);
  println!("[{title}] {body}");
}

/// Send a notification and log to stderr.
pub fn log_notify_err(title: &str, body: impl AsRef<str>, e: impl Into<anyhow::Error>) {
  let body = body.as_ref();
  let e = e.into();
  notify(title, format!("{body}: {e}"));
  eprintln!("[ERROR]:\n{title}: {body}\n{e:#}");
}

/// Send a notification and return an error.
pub fn bail_notify<T>(title: &str, body: impl AsRef<str>) -> anyhow::Result<T> {
  let body = body.as_ref();
  notify(title, body);
  anyhow::bail!("[ERROR]:\n{title}: {body}")
}

/// Send a notification and return an error, with additional error context.
pub fn bail_notify_err<T>(
  title: &str,
  body: impl AsRef<str>,
  e: impl Into<anyhow::Error>,
) -> anyhow::Result<T> {
  let body = body.as_ref();
  let e = e.into();
  notify(title, format!("{body}: {e}"));
  anyhow::bail!("[ERROR]:\n{title}: {body}\n{e:#}")
}

/// Resolve a device path to a block device managed by UDisks2. Returns None if not found.
pub fn find_block_device(
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

/// Print summary about a device.
pub fn print_device_info(info: &DeviceInfo, mount_options: &HashMap<&str, Value<'_>>) {
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
    None => "Filesystem:         -".to_string(),
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

/// Print all UDisks2 properties for all Interfaces.
pub fn print_udisks_properties(
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
