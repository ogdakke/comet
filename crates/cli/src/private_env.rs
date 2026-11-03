//! A personal deployment's edge and WorkOS application, read from the `env`
//! file beside the active runtime's state. (Fork: there is no baked
//! production edge or client id.)

use std::path::PathBuf;

use crate::paths;

const PRIVATE_ENV_KEYS: [&str; 2] = ["ZERON_EDGE_URL", "ZERON_WORKOS_CLIENT_ID"];

/// Load the deployment file's values that the environment doesn't already set.
/// Call before any other thread starts.
pub fn load() {
    let Ok(contents) = std::fs::read_to_string(env_path_for(paths::data_dir())) else {
        return;
    };

    for (key, value) in parse(&contents) {
        if std::env::var_os(key).is_none() {
            // This is before CLI parsing, logging, or runtime creation, so no
            // other thread can observe a partially loaded process environment.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

/// The deployment file lives beside the active runtime's state. This keeps a
/// source build (`~/.zeron-dev`) from inheriting the installed app's edge and
/// WorkOS application (`~/.zeron`). An explicit `ZERON_DATA_DIR` gets the same
/// behavior, which makes private test deployments self-contained.
fn env_path_for(data_dir: PathBuf) -> PathBuf {
    data_dir.join("env")
}

fn parse(contents: &str) -> Vec<(&'static str, String)> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let key = PRIVATE_ENV_KEYS
                .iter()
                .copied()
                .find(|known| *known == key)?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            (!value.is_empty()).then(|| (key, value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{env_path_for, parse};

    #[test]
    fn parses_only_private_deployment_values() {
        let values = parse(
            "# personal deployment\nZERON_EDGE_URL=https://edge.example\nZERON_WORKOS_CLIENT_ID=\"client_test\"\nZERON_EDGE_TOKEN=must-not-load\n",
        );
        assert_eq!(
            values,
            vec![
                ("ZERON_EDGE_URL", "https://edge.example".into()),
                ("ZERON_WORKOS_CLIENT_ID", "client_test".into()),
            ]
        );
    }

    #[test]
    fn private_env_follows_the_active_data_directory() {
        assert_eq!(
            env_path_for(std::path::PathBuf::from("/tmp/zeron-isolated")),
            std::path::PathBuf::from("/tmp/zeron-isolated/env")
        );
    }
}
