use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

/// Raw config file schema. Unknown keys are ignored (serde default).
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(rename = "KLIPY_API_KEY")]
    klipy_api_key: Option<String>,
    #[serde(rename = "GIPHY_API_KEY")]
    giphy_api_key: Option<String>,
    #[serde(rename = "GIFDECK_FAVORITES_API")]
    favorites_api: Option<String>,
    #[serde(rename = "GIFDECK_FAVORITES_TOKEN")]
    favorites_token: Option<String>,
}

/// Resolved runtime configuration.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub klipy_api_key: Option<String>,
    pub giphy_api_key: Option<String>,
    pub favorites_api: Option<String>,
    pub favorites_token: Option<String>,
}

/// Canonical keys that can be overridden via environment variables.
const ENV_KEYS: [&str; 4] = [
    "KLIPY_API_KEY",
    "GIPHY_API_KEY",
    "GIFDECK_FAVORITES_API",
    "GIFDECK_FAVORITES_TOKEN",
];

/// Default favorites API base when not configured.
pub const DEFAULT_FAVORITES_API: &str = "https://favs.veryshiny.net/api/v1";

/// Resolved once per process.
static CONFIG: OnceLock<Config> = OnceLock::new();

/// Path to the config file, honoring the `GIFDECK_CONFIG` override.
pub fn config_path() -> PathBuf {
    if let Some(p) = env::var_os("GIFDECK_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    match dirs::config_dir() {
        Some(dir) => dir.join("gifdeck").join("config.json"),
        None => PathBuf::from("config.json"),
    }
}

/// Returns the process-wide config, loading it on first call.
pub fn config() -> &'static Config {
    CONFIG.get_or_init(load)
}

/// Reads and resolves configuration.
///
/// Precedence: non-empty env var wins, then file value, then unset.
/// A missing file is not an error. Invalid JSON warns to stderr and
/// continues with an empty config. Empty values are treated as unset.
/// Values are never logged in the clear.
fn load() -> Config {
    let mut cfg = Config::default();
    let path = config_path();

    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<RawConfig>(&contents) {
            Ok(raw) => {
                cfg.klipy_api_key = non_empty(raw.klipy_api_key);
                cfg.giphy_api_key = non_empty(raw.giphy_api_key);
                cfg.favorites_api = non_empty(raw.favorites_api);
                cfg.favorites_token = non_empty(raw.favorites_token);
            }
            Err(e) => {
                eprintln!(
                    "gifdeck: warning: invalid config JSON at {}: {}. Continuing with empty config.",
                    path.display(),
                    e
                );
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Missing file is fine: empty config.
        }
        Err(e) => {
            eprintln!(
                "gifdeck: warning: could not read config at {}: {}. Continuing with empty config.",
                path.display(),
                e
            );
        }
    }

    // Env overrides win.
    for key in ENV_KEYS {
        if let Ok(val) = env::var(key) {
            if !val.is_empty() {
                match key {
                    "KLIPY_API_KEY" => cfg.klipy_api_key = Some(val),
                    "GIPHY_API_KEY" => cfg.giphy_api_key = Some(val),
                    "GIFDECK_FAVORITES_API" => cfg.favorites_api = Some(val),
                    "GIFDECK_FAVORITES_TOKEN" => cfg.favorites_token = Some(val),
                    _ => unreachable!(),
                }
            }
        }
    }

    cfg
}

/// `Some` only for non-blank values (empty file values are unset).
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

impl Config {
    /// Favorites API base URL.
    pub fn favorites_api(&self) -> &str {
        self.favorites_api
            .as_deref()
            .unwrap_or(DEFAULT_FAVORITES_API)
    }

    /// Whether favorites live on the self-hosted server. When no token is
    /// configured, favorites use the local store instead — the two are
    /// exclusive, never a fallback for one another.
    pub fn use_server_favorites(&self) -> bool {
        self.favorites_token.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, MutexGuard};

    fn temp_config(contents: &str) -> PathBuf {
        let dir = tempfile_dir();
        let path = dir.join("config.json");
        fs::write(&path, contents).unwrap();
        path
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gifdeck-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn clear_env() {
        for key in ENV_KEYS {
            env::remove_var(key);
        }
        // Legacy gifgrep env vars must not leak into any test.
        env::remove_var("GIFGREP_FAVORITES_API");
        env::remove_var("GIFGREP_FAVORITES_TOKEN");
        env::remove_var("GIFDECK_CONFIG");
    }

    /// Serializes env-var-mutating tests to avoid cross-test races.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn missing_file_is_empty_config() {
        let _guard = env_lock();
        clear_env();
        env::set_var("GIFDECK_CONFIG", "/nonexistent/does-not-exist.json");
        let cfg = load();
        assert_eq!(cfg.klipy_api_key, None);
        assert_eq!(cfg.favorites_api, None);
        assert!(!cfg.use_server_favorites());
    }

    #[test]
    fn invalid_json_warns_and_returns_empty() {
        let _guard = env_lock();
        clear_env();
        let path = temp_config("{ not valid json ");
        env::set_var("GIFDECK_CONFIG", &path);
        let cfg = load();
        assert_eq!(cfg.klipy_api_key, None);
    }

    #[test]
    fn reads_file_values() {
        let _guard = env_lock();
        clear_env();
        let path = temp_config(
            r#"{"KLIPY_API_KEY":"filek","GIPHY_API_KEY":"fileg","GIFDECK_FAVORITES_API":"http://file/api","GIFDECK_FAVORITES_TOKEN":"filet"}"#,
        );
        env::set_var("GIFDECK_CONFIG", &path);
        let cfg = load();
        assert_eq!(cfg.klipy_api_key.as_deref(), Some("filek"));
        assert_eq!(cfg.giphy_api_key.as_deref(), Some("fileg"));
        assert_eq!(cfg.favorites_api.as_deref(), Some("http://file/api"));
        assert_eq!(cfg.favorites_token.as_deref(), Some("filet"));
        assert!(cfg.use_server_favorites());
    }

    #[test]
    fn env_overrides_file() {
        let _guard = env_lock();
        clear_env();
        let path = temp_config(
            r#"{"KLIPY_API_KEY":"filek","GIFDECK_FAVORITES_API":"http://file/api"}"#,
        );
        env::set_var("GIFDECK_CONFIG", &path);
        env::set_var("KLIPY_API_KEY", "envk");
        env::set_var("GIFDECK_FAVORITES_API", "http://env/api");
        let cfg = load();
        assert_eq!(cfg.klipy_api_key.as_deref(), Some("envk"));
        assert_eq!(cfg.favorites_api.as_deref(), Some("http://env/api"));
        assert_eq!(cfg.giphy_api_key, None);
    }

    #[test]
    fn legacy_gifgrep_keys_are_ignored() {
        let _guard = env_lock();
        clear_env();
        // Old gifgrep names in the file must not resolve to anything.
        let path = temp_config(
            r#"{"GIFGREP_FAVORITES_API":"http://legacy/api","GIFGREP_FAVORITES_TOKEN":"legacyt"}"#,
        );
        env::set_var("GIFDECK_CONFIG", &path);
        env::set_var("GIFGREP_FAVORITES_TOKEN", "envlegacy");
        let cfg = load();
        assert_eq!(cfg.favorites_api, None, "legacy API key must be ignored");
        assert_eq!(cfg.favorites_token, None, "legacy token must be ignored");
        assert!(!cfg.use_server_favorites());
        assert_eq!(cfg.favorites_api(), DEFAULT_FAVORITES_API);
    }

    #[test]
    fn unknown_json_keys_ignored() {
        let _guard = env_lock();
        clear_env();
        let path = temp_config(r#"{"SOMETHING_ELSE":"x","KLIPY_API_KEY":"k","unknown":1}"#);
        env::set_var("GIFDECK_CONFIG", &path);
        let cfg = load();
        assert_eq!(cfg.klipy_api_key.as_deref(), Some("k"));
    }

    #[test]
    fn empty_env_does_not_override() {
        let _guard = env_lock();
        clear_env();
        let path = temp_config(r#"{"KLIPY_API_KEY":"filek"}"#);
        env::set_var("GIFDECK_CONFIG", &path);
        env::set_var("KLIPY_API_KEY", "");
        let cfg = load();
        assert_eq!(cfg.klipy_api_key.as_deref(), Some("filek"));
    }

    #[test]
    fn blank_file_values_are_unset() {
        let _guard = env_lock();
        clear_env();
        let path = temp_config(
            r#"{"KLIPY_API_KEY":"  ","GIFDECK_FAVORITES_TOKEN":""}"#,
        );
        env::set_var("GIFDECK_CONFIG", &path);
        let cfg = load();
        assert_eq!(cfg.klipy_api_key, None);
        assert_eq!(cfg.favorites_token, None, "blank token means local mode");
        assert!(!cfg.use_server_favorites());
    }

    #[test]
    fn gifdeck_config_env_overrides_default_path() {
        let _guard = env_lock();
        clear_env();
        env::set_var("GIFDECK_CONFIG", "/some/alt/file.json");
        assert_eq!(config_path(), PathBuf::from("/some/alt/file.json"));
    }

    #[test]
    fn default_path_lives_in_gifdeck_dir() {
        let _guard = env_lock();
        clear_env();
        let path = config_path();
        let in_gifdeck = path
            .components()
            .any(|c| c == std::path::Component::Normal(std::ffi::OsStr::new("gifdeck")));
        assert!(in_gifdeck, "config lives under a gifdeck directory: {path:?}");
        assert!(
            !path.to_string_lossy().contains("gifgrep"),
            "no gifgrep references: {path:?}"
        );
    }
}