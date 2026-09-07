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
    #[serde(rename = "FAVORITES_MODE")]
    favorites_mode: Option<String>,
}

/// Resolved runtime configuration.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub klipy_api_key: Option<String>,
    pub giphy_api_key: Option<String>,
    pub favorites_api: Option<String>,
    pub favorites_token: Option<String>,
    /// Explicit favorites-mode override: `"server"` or `"local"`.
    pub favorites_mode: Option<String>,
}

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

/// Reads and resolves configuration from the config file.
///
/// The file is the only source of configuration values. A missing file
/// is not an error. Invalid JSON warns to stderr and continues with an
/// empty config. Empty/blank values are treated as unset. Values are
/// never logged in the clear.
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
                cfg.favorites_mode = favorites_mode(&raw.favorites_mode);
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

    cfg
}

/// `Some` only for non-blank values (empty file values are unset).
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Validate and normalize the `FAVORITES_MODE` key. Returns `None` for
/// blank values and warns on unknown values (treated as unset, so mode
/// falls back to token presence).
fn favorites_mode(raw: &Option<String>) -> Option<String> {
    let value = raw.as_deref()?.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    if value != "server" && value != "local" {
        eprintln!(
            "gifdeck: warning: unknown FAVORITES_MODE value; expected \"server\" or \"local\". \
             Falling back to token-based mode."
        );
        return None;
    }
    Some(value)
}

impl Config {
    /// Favorites API base URL, when configured. There is no default —
    /// server mode requires an explicit `GIFDECK_FAVORITES_API`.
    pub fn favorites_api(&self) -> Option<&str> {
        self.favorites_api.as_deref()
    }

    /// Whether favorites live on the self-hosted server. An explicit
    /// `FAVORITES_MODE` overrides the token heuristic: `"server"` forces
    /// server mode (even without a token — requests just go unauthenticated
    /// and server errors surface in the footer), `"local"` forces the local
    /// store (even when a token is configured). Otherwise mode is decided
    /// by token presence. The two stores are exclusive, never a fallback
    /// for one another.
    pub fn use_server_favorites(&self) -> bool {
        match self.favorites_mode.as_deref() {
            Some("local") => false,
            Some("server") => true,
            _ => self.favorites_token.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

    /// Serializes `GIFDECK_CONFIG`-mutating tests to avoid cross-test races.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap()
    }

    fn with_config(contents: &str) -> PathBuf {
        let path = temp_config(contents);
        env::set_var("GIFDECK_CONFIG", &path);
        path
    }

    #[test]
    fn missing_file_is_empty_config() {
        let _guard = env_lock();
        env::set_var("GIFDECK_CONFIG", "/nonexistent/does-not-exist.json");
        let cfg = load();
        assert_eq!(cfg.klipy_api_key, None);
        assert_eq!(cfg.favorites_api, None);
        assert!(!cfg.use_server_favorites());
    }

    #[test]
    fn invalid_json_warns_and_returns_empty() {
        let _guard = env_lock();
        let _path = with_config("{ not valid json ");
        let cfg = load();
        assert_eq!(cfg.klipy_api_key, None);
    }

    #[test]
    fn reads_file_values() {
        let _guard = env_lock();
        let _path = with_config(
            r#"{"KLIPY_API_KEY":"filek","GIPHY_API_KEY":"fileg","GIFDECK_FAVORITES_API":"http://file/api","GIFDECK_FAVORITES_TOKEN":"filet"}"#,
        );
        let cfg = load();
        assert_eq!(cfg.klipy_api_key.as_deref(), Some("filek"));
        assert_eq!(cfg.giphy_api_key.as_deref(), Some("fileg"));
        assert_eq!(cfg.favorites_api.as_deref(), Some("http://file/api"));
        assert_eq!(cfg.favorites_token.as_deref(), Some("filet"));
        assert!(cfg.use_server_favorites());
    }

    #[test]
    fn unknown_json_keys_ignored() {
        let _guard = env_lock();
        let _path = with_config(r#"{"SOMETHING_ELSE":"x","KLIPY_API_KEY":"k","unknown":1}"#);
        let cfg = load();
        assert_eq!(cfg.klipy_api_key.as_deref(), Some("k"));
    }

    #[test]
    fn blank_file_values_are_unset() {
        let _guard = env_lock();
        let _path = with_config(r#"{"KLIPY_API_KEY":"  ","GIFDECK_FAVORITES_TOKEN":""}"#);
        let cfg = load();
        assert_eq!(cfg.klipy_api_key, None);
        assert_eq!(cfg.favorites_token, None, "blank token means local mode");
        assert!(!cfg.use_server_favorites());
    }

    #[test]
    fn gifdeck_config_env_overrides_default_path() {
        let _guard = env_lock();
        env::set_var("GIFDECK_CONFIG", "/some/alt/file.json");
        assert_eq!(config_path(), PathBuf::from("/some/alt/file.json"));
    }

    #[test]
    fn default_path_lives_in_gifdeck_dir() {
        let _guard = env_lock();
        env::remove_var("GIFDECK_CONFIG");
        let path = config_path();
        let in_gifdeck = path
            .components()
            .any(|c| c == std::path::Component::Normal(std::ffi::OsStr::new("gifdeck")));
        assert!(
            in_gifdeck,
            "config lives under a gifdeck directory: {path:?}"
        );
        assert!(
            !path.to_string_lossy().contains("gifgrep"),
            "no gifgrep references: {path:?}"
        );
    }

    #[test]
    fn favorites_mode_local_overrides_token() {
        let _guard = env_lock();
        let _path = with_config(r#"{"GIFDECK_FAVORITES_TOKEN":"tok","FAVORITES_MODE":"local"}"#);
        let cfg = load();
        assert_eq!(cfg.favorites_mode.as_deref(), Some("local"));
        assert!(
            !cfg.use_server_favorites(),
            "explicit local mode wins over a configured token"
        );
    }

    #[test]
    fn favorites_mode_server_overrides_missing_token() {
        let _guard = env_lock();
        let _path = with_config(r#"{"FAVORITES_MODE":"server"}"#);
        let cfg = load();
        assert_eq!(cfg.favorites_mode.as_deref(), Some("server"));
        assert!(
            cfg.use_server_favorites(),
            "explicit server mode wins over a missing token"
        );
    }

    #[test]
    fn favorites_mode_is_case_insensitive_and_trimmed() {
        let _guard = env_lock();
        let _path = with_config(r#"{"FAVORITES_MODE":"  Local "}"#);
        let cfg = load();
        assert_eq!(cfg.favorites_mode.as_deref(), Some("local"));
        assert!(!cfg.use_server_favorites());
    }

    #[test]
    fn favorites_mode_invalid_falls_back_to_token_presence() {
        let _guard = env_lock();
        let _path = with_config(r#"{"GIFDECK_FAVORITES_TOKEN":"tok","FAVORITES_MODE":"neither"}"#);
        let cfg = load();
        assert_eq!(cfg.favorites_mode, None, "invalid mode is unset");
        assert!(cfg.use_server_favorites(), "falls back to token presence");
    }

    #[test]
    fn favorites_mode_blank_is_unset() {
        let _guard = env_lock();
        let _path = with_config(r#"{"FAVORITES_MODE":"   "}"#);
        let cfg = load();
        assert_eq!(cfg.favorites_mode, None);
        assert!(!cfg.use_server_favorites());
    }

    #[test]
    fn favorites_mode_unknown_keys_ignored_test_placeholder() {
        // Guards against accidentally treating legacy unknown keys as mode.
        let _guard = env_lock();
        let _path = with_config(r#"{"GIFGREP_FAVORITES_TOKEN":"legacyt"}"#);
        let cfg = load();
        assert_eq!(cfg.favorites_token, None, "legacy keys are not read");
        assert_eq!(cfg.favorites_mode, None);
    }
}
