use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

/// Resolves config path. Order of precendence is CLI -> `XDG_CONFIG_HOME` -> /etc
fn resolve_path(path: Option<&Path>) -> Option<PathBuf> {
  if let Some(p) = path
    && p.exists()
  {
    return Some(p.to_path_buf());
  }

  if let Ok(mut user_config) = env::var("XDG_CONFIG_HOME") {
    user_config.push_str("udiskrs/mounts.conf");
    let p = PathBuf::from(user_config);
    if p.exists() {
      return Some(p);
    }
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

    let parts = line.split_whitespace().collect::<Vec<&str>>();

    if parts.len() == 2 {
      map.insert(parts[0].trim().to_string(), parts[1].trim().to_string());
    }
  }
  map
}
