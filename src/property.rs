use std::collections::HashMap;
use std::path::PathBuf;

use zbus::names::{InterfaceName, OwnedInterfaceName};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

trait AsZVariantValue {
  fn as_value(&self) -> &Value<'_>;
}

impl AsZVariantValue for OwnedValue {
  fn as_value(&self) -> &Value<'_> {
    self
  }
}

impl AsZVariantValue for Value<'_> {
  fn as_value(&self) -> &Value<'_> {
    self
  }
}

pub trait PropertyMap {
  fn get_value(&self, key: &str) -> Option<&Value<'_>>;

  fn get_str(&self, key: &str) -> Option<String> {
    self
      .get_value(key)
      .and_then(|v| <&str>::try_from(v).ok())
      .map(String::from)
  }

  fn get_bool(&self, key: &str) -> bool {
    self
      .get_value(key)
      .and_then(|v| bool::try_from(v).ok())
      .unwrap_or(false)
  }

  fn get_path(&self, key: &str) -> Option<PathBuf> {
    let val = self.get_value(key)?;
    let bytes: Result<Vec<u8>, _> = val.clone().try_into();
    bytes.ok().map(|b| parse_byte_path(&b))
  }

  fn get_paths(&self, key: &str) -> Vec<PathBuf> {
    let Some(val) = self.get_value(key) else {
      return Vec::new();
    };
    let entries: Result<Vec<Vec<u8>>, _> = val.clone().try_into();
    entries
      .map(|list| list.into_iter().map(|b| parse_byte_path(&b)).collect())
      .unwrap_or_default()
  }

  fn get_object_path(&self, key: &str) -> Option<OwnedObjectPath> {
    self
      .get_value(key)
      .and_then(|v| OwnedObjectPath::try_from(v.clone()).ok())
  }
}

impl<K, V> PropertyMap for HashMap<K, V>
where
  K: std::borrow::Borrow<str> + std::hash::Hash + Eq,
  V: AsZVariantValue,
{
  fn get_value(&self, key: &str) -> Option<&Value<'_>> {
    self.get(key).map(|v| v.as_value())
  }
}

pub trait InterfaceMap {
  type Props: PropertyMap;

  fn get_iface(&self, name: &str) -> Option<&Self::Props>;

  fn has_iface(&self, name: &str) -> bool {
    self.get_iface(name).is_some()
  }
}

// ManagedObjects sweep map (startup sweep)
impl InterfaceMap for HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>> {
  type Props = HashMap<String, OwnedValue>;

  fn get_iface(&self, name: &str) -> Option<&Self::Props> {
    self
      .iter()
      .find(|(iface, _)| iface.as_str() == name)
      .map(|(_, props)| props)
  }
}

// InterfacesAdded signal map (runtime events)
impl<'a> InterfaceMap for HashMap<InterfaceName<'a>, HashMap<&'a str, Value<'a>>> {
  type Props = HashMap<&'a str, Value<'a>>;

  fn get_iface(&self, name: &str) -> Option<&Self::Props> {
    self
      .iter()
      .find(|(iface, _)| iface.as_str() == name)
      .map(|(_, props)| props)
  }
}

fn parse_byte_path(bytes: &[u8]) -> PathBuf {
  let string = String::from_utf8_lossy(bytes);
  PathBuf::from(string.trim_end_matches('\0'))
}
