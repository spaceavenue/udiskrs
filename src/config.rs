use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

/// Resolves config path. Order of precedence is CLI -> `XDG_CONFIG_HOME` -> `$HOME/.config` ->
/// `/etc`
fn resolve_path(path: Option<&Path>) -> Option<PathBuf> {
  if let Some(p) = path.filter(|p| p.exists()) {
    return Some(p.to_path_buf());
  }

  let user_config = env::var_os("XDG_CONFIG_HOME")
    .map(PathBuf::from)
    .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    .map(|p| p.join("udiskrs/mounts.conf"));

  if let Some(p) = user_config.filter(|p| p.exists()) {
    return Some(p);
  }

  let system_config = PathBuf::from("/etc/udiskrs/mounts.conf");
  if system_config.exists() {
    return Some(system_config);
  }

  None
}

/// Loads the config at `path` and parses it.
pub fn load_config(path: Option<&Path>) -> HashMap<String, String> {
  let mut map = HashMap::new();

  let Some(target_path) = resolve_path(path) else {
    return map;
  };

  let Ok(content) = std::fs::read_to_string(target_path) else {
    return map;
  };

  for line in content.lines().map(str::trim) {
    if line.is_empty() || line.starts_with('#') {
      continue;
    }

    if let Some((key, val)) = line.split_once(char::is_whitespace) {
      map.insert(key.trim().to_string(), val.trim().to_string());
    }
  }
  map
}
