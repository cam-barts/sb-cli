use crate::error::{SbError, SbResult};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tracing::debug;

/// Represents the raw TOML config file structure at `.sb/config.toml`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ConfigFile {
    pub server_url: Option<String>,
    pub token: Option<String>,
    #[serde(default)]
    pub sync: SyncConfigFile,
    #[serde(default)]
    pub daily: DailyConfigFile,
    #[serde(default)]
    pub shell: ShellConfigFile,
    #[serde(default)]
    pub auth: AuthConfigFile,
    #[serde(default)]
    pub runtime: RuntimeConfigFile,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct SyncConfigFile {
    pub dir: Option<String>,
    pub workers: Option<u32>,
    pub attachments: Option<bool>,
    pub exclude: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct DailyConfigFile {
    pub path: Option<String>,
    #[serde(rename = "dateFormat")]
    pub date_format: Option<String>,
    pub template: Option<String>,
    #[serde(rename = "timeFormat")]
    pub time_format: Option<String>,
    #[serde(rename = "bulletStyle")]
    pub bullet_style: Option<String>,
    #[serde(rename = "taskTag")]
    pub task_tag: Option<String>,
    #[serde(rename = "taskTagMode")]
    pub task_tag_mode: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ShellConfigFile {
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct AuthConfigFile {
    pub keychain: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct RuntimeConfigFile {
    pub available: Option<bool>,
}

/// Tracks where a configuration value came from.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigSource {
    Default,
    /// Value came from the per-space `<space>/.sb/config.toml`.
    File,
    /// Value came from the user-level XDG config `~/.config/sb/config.toml`.
    UserFile,
    Env(String),
    /// Used when a CLI flag overrides this config value (reserved for future use).
    #[allow(dead_code)]
    Flag(String),
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::Default => write!(f, "default"),
            ConfigSource::File => write!(f, "config"),
            ConfigSource::UserFile => write!(f, "user config"),
            ConfigSource::Env(name) => write!(f, "env: {name}"),
            ConfigSource::Flag(name) => write!(f, "flag: {name}"),
        }
    }
}

/// A config value paired with its resolution source.
#[derive(Debug, Clone)]
pub struct ResolvedValue<T> {
    pub value: T,
    pub source: ConfigSource,
}

impl<T> ResolvedValue<T> {
    pub fn new(value: T, source: ConfigSource) -> Self {
        Self { value, source }
    }
}

/// Fully resolved configuration with source tracking on every field.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub server_url: ResolvedValue<Option<String>>,
    pub token: ResolvedValue<Option<String>>,
    pub sync_dir: ResolvedValue<String>,
    pub sync_workers: ResolvedValue<u32>,
    pub sync_attachments: ResolvedValue<bool>,
    pub sync_exclude: ResolvedValue<Vec<String>>,
    pub sync_include: ResolvedValue<Vec<String>>,
    pub daily_path: ResolvedValue<String>,
    pub daily_date_format: ResolvedValue<String>,
    pub daily_template: ResolvedValue<Option<String>>,
    pub daily_time_format: ResolvedValue<String>,
    pub daily_bullet_style: ResolvedValue<String>,
    pub daily_task_tag: ResolvedValue<String>,
    pub daily_task_tag_mode: ResolvedValue<String>,
    pub shell_enabled: ResolvedValue<bool>,
    pub auth_keychain: ResolvedValue<bool>,
    pub runtime_available: ResolvedValue<bool>,
}

/// Mask an auth token for display.
///
/// - Tokens with 6+ chars: show first 3 + "..." + last 3 (e.g. "sk-...456")
/// - Tokens shorter than 6 chars but non-empty: "***"
/// - Empty tokens: ""
pub fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let len = chars.len();
    if len == 0 {
        String::new()
    } else if len < 6 {
        "***".to_string()
    } else {
        let prefix: String = chars[..3].iter().collect();
        let suffix: String = chars[len - 3..].iter().collect();
        format!("{prefix}...{suffix}")
    }
}

/// Find the `.sb/config.toml` file by searching current directory and parents.
pub fn find_config_file(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join(".sb").join("config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Parse a bool from an env var value: accepts "true"/"1" as true, "false"/"0" as false.
fn parse_env_bool(val: &str) -> Result<bool, String> {
    match val.to_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(format!("invalid boolean value: {other}")),
    }
}

/// Parse a comma-separated list from an env var value.
fn parse_env_vec(val: &str) -> Vec<String> {
    if val.is_empty() {
        vec![]
    } else {
        val.split(',').map(|s| s.trim().to_string()).collect()
    }
}

impl ResolvedConfig {
    /// Load configuration by resolving: defaults <- XDG config <- per-space config <- env vars.
    ///
    /// CLI flag overrides are applied separately by the caller since they
    /// depend on parsed CLI arguments.
    pub fn load() -> SbResult<Self> {
        let cwd = std::env::current_dir().map_err(|e| SbError::Config {
            message: format!("cannot determine current directory: {e}"),
        })?;
        Self::load_from(&cwd)
    }

    /// Load configuration starting from a specific directory.
    ///
    /// Resolves with precedence: env > per-space config > XDG user config > defaults.
    /// Loading the per-space config is best-effort (defaults are used if missing).
    pub fn load_from(start_dir: &Path) -> SbResult<Self> {
        // Step 1: Try to find and parse the per-space config file
        let config_file = if let Some(path) = find_config_file(start_dir) {
            debug!("per-space config file found: {}", path.display());
            let content = std::fs::read_to_string(&path).map_err(|e| SbError::Config {
                message: format!("cannot read {}: {e}", path.display()),
            })?;
            toml::from_str::<ConfigFile>(&content).map_err(|e| SbError::Config {
                message: format!("invalid config at {}: {e}", path.display()),
            })?
        } else {
            debug!("no per-space config file found");
            ConfigFile::default()
        };

        // Step 2: Load the XDG user config (acts as a fallback when per-space
        // doesn't define a field). Reading errors are propagated; absence is
        // treated as default.
        let user_config = load_user_config()?;

        // Step 3: Resolve each field with precedence: env > per-space > XDG > default
        let server_url = Self::resolve_optional_string(
            "SB_SERVER_URL",
            config_file.server_url,
            user_config.server_url,
        );
        let token = Self::resolve_optional_string("SB_TOKEN", config_file.token, user_config.token);

        let sync_dir = Self::resolve_string(
            "SB_SYNC_DIR",
            config_file.sync.dir,
            user_config.sync.dir,
            "space".to_string(),
        );
        let sync_workers = Self::resolve_u32(
            "SB_SYNC_WORKERS",
            config_file.sync.workers,
            user_config.sync.workers,
            4,
        )?;
        let sync_attachments = Self::resolve_bool(
            "SB_SYNC_ATTACHMENTS",
            config_file.sync.attachments,
            user_config.sync.attachments,
            false,
        )?;
        let sync_exclude = Self::resolve_vec(
            "SB_SYNC_EXCLUDE",
            config_file.sync.exclude,
            user_config.sync.exclude,
            vec!["_plug/*".to_string()],
        );
        let sync_include = Self::resolve_vec(
            "SB_SYNC_INCLUDE",
            config_file.sync.include,
            user_config.sync.include,
            vec![],
        );

        let daily_path = Self::resolve_string(
            "SB_DAILY_PATH",
            config_file.daily.path,
            user_config.daily.path,
            "Journal/{{date}}".to_string(),
        );
        let daily_date_format = Self::resolve_string(
            "SB_DAILY_DATE_FORMAT",
            config_file.daily.date_format,
            user_config.daily.date_format,
            "%Y-%m-%d".to_string(),
        );
        let daily_template = Self::resolve_optional_string(
            "SB_DAILY_TEMPLATE",
            config_file.daily.template,
            user_config.daily.template,
        );
        let daily_time_format = Self::resolve_string(
            "SB_DAILY_TIME_FORMAT",
            config_file.daily.time_format,
            user_config.daily.time_format,
            "%H:%M".to_string(),
        );
        let daily_bullet_style = Self::resolve_string(
            "SB_DAILY_BULLET_STYLE",
            config_file.daily.bullet_style,
            user_config.daily.bullet_style,
            "*".to_string(),
        );
        if daily_bullet_style.value != "*" && daily_bullet_style.value != "-" {
            return Err(SbError::Config {
                message: format!(
                    "invalid daily.bulletStyle: {:?} (expected \"*\" or \"-\")",
                    daily_bullet_style.value
                ),
            });
        }
        let daily_task_tag = Self::resolve_string(
            "SB_DAILY_TASK_TAG",
            config_file.daily.task_tag,
            user_config.daily.task_tag,
            "task".to_string(),
        );
        let daily_task_tag_mode = Self::resolve_string(
            "SB_DAILY_TASK_TAG_MODE",
            config_file.daily.task_tag_mode,
            user_config.daily.task_tag_mode,
            "auto".to_string(),
        );
        if !matches!(
            daily_task_tag_mode.value.as_str(),
            "auto" | "always" | "never"
        ) {
            return Err(SbError::Config {
                message: format!(
                    "invalid daily.taskTagMode: {:?} (expected \"auto\", \"always\", or \"never\")",
                    daily_task_tag_mode.value
                ),
            });
        }

        let shell_enabled = Self::resolve_bool(
            "SB_SHELL_ENABLED",
            config_file.shell.enabled,
            user_config.shell.enabled,
            false,
        )?;
        let auth_keychain = Self::resolve_bool(
            "SB_AUTH_KEYCHAIN",
            config_file.auth.keychain,
            user_config.auth.keychain,
            false,
        )?;
        let runtime_available = Self::resolve_bool(
            "SB_RUNTIME_AVAILABLE",
            config_file.runtime.available,
            user_config.runtime.available,
            false,
        )?;

        Ok(ResolvedConfig {
            server_url,
            token,
            sync_dir,
            sync_workers,
            sync_attachments,
            sync_exclude,
            sync_include,
            daily_path,
            daily_date_format,
            daily_template,
            daily_time_format,
            daily_bullet_style,
            daily_task_tag,
            daily_task_tag_mode,
            shell_enabled,
            auth_keychain,
            runtime_available,
        })
    }

    fn resolve_optional_string(
        env_key: &str,
        file_val: Option<String>,
        xdg_val: Option<String>,
    ) -> ResolvedValue<Option<String>> {
        if let Ok(val) = std::env::var(env_key) {
            ResolvedValue::new(Some(val), ConfigSource::Env(env_key.to_string()))
        } else if let Some(val) = file_val {
            ResolvedValue::new(Some(val), ConfigSource::File)
        } else if let Some(val) = xdg_val {
            ResolvedValue::new(Some(val), ConfigSource::UserFile)
        } else {
            ResolvedValue::new(None, ConfigSource::Default)
        }
    }

    fn resolve_string(
        env_key: &str,
        file_val: Option<String>,
        xdg_val: Option<String>,
        default: String,
    ) -> ResolvedValue<String> {
        if let Ok(val) = std::env::var(env_key) {
            ResolvedValue::new(val, ConfigSource::Env(env_key.to_string()))
        } else if let Some(val) = file_val {
            ResolvedValue::new(val, ConfigSource::File)
        } else if let Some(val) = xdg_val {
            ResolvedValue::new(val, ConfigSource::UserFile)
        } else {
            ResolvedValue::new(default, ConfigSource::Default)
        }
    }

    fn resolve_u32(
        env_key: &str,
        file_val: Option<u32>,
        xdg_val: Option<u32>,
        default: u32,
    ) -> SbResult<ResolvedValue<u32>> {
        if let Ok(val) = std::env::var(env_key) {
            let parsed = val.parse::<u32>().map_err(|_| SbError::Config {
                message: format!("invalid value for {env_key}: expected integer, got \"{val}\""),
            })?;
            Ok(ResolvedValue::new(
                parsed,
                ConfigSource::Env(env_key.to_string()),
            ))
        } else if let Some(val) = file_val {
            Ok(ResolvedValue::new(val, ConfigSource::File))
        } else if let Some(val) = xdg_val {
            Ok(ResolvedValue::new(val, ConfigSource::UserFile))
        } else {
            Ok(ResolvedValue::new(default, ConfigSource::Default))
        }
    }

    fn resolve_bool(
        env_key: &str,
        file_val: Option<bool>,
        xdg_val: Option<bool>,
        default: bool,
    ) -> SbResult<ResolvedValue<bool>> {
        if let Ok(val) = std::env::var(env_key) {
            let parsed = parse_env_bool(&val).map_err(|e| SbError::Config {
                message: format!("invalid value for {env_key}: {e}"),
            })?;
            Ok(ResolvedValue::new(
                parsed,
                ConfigSource::Env(env_key.to_string()),
            ))
        } else if let Some(val) = file_val {
            Ok(ResolvedValue::new(val, ConfigSource::File))
        } else if let Some(val) = xdg_val {
            Ok(ResolvedValue::new(val, ConfigSource::UserFile))
        } else {
            Ok(ResolvedValue::new(default, ConfigSource::Default))
        }
    }

    fn resolve_vec(
        env_key: &str,
        file_val: Option<Vec<String>>,
        xdg_val: Option<Vec<String>>,
        default: Vec<String>,
    ) -> ResolvedValue<Vec<String>> {
        if let Ok(val) = std::env::var(env_key) {
            ResolvedValue::new(parse_env_vec(&val), ConfigSource::Env(env_key.to_string()))
        } else if let Some(val) = file_val {
            ResolvedValue::new(val, ConfigSource::File)
        } else if let Some(val) = xdg_val {
            ResolvedValue::new(val, ConfigSource::UserFile)
        } else {
            ResolvedValue::new(default, ConfigSource::Default)
        }
    }
}

/// Resolve the auth token.
///
/// Precedence: CLI flag > env > keychain > per-space config > XDG user config.
///
/// The keychain lookup is only attempted when `config.auth_keychain.value` is true
/// and a server_url is available (needed as keychain entry identifier).
///
/// Returns `Err(SbError::TokenNotFound)` when no source provides a non-empty token.
pub fn resolve_token(cli_flag: Option<&str>, config: &ResolvedConfig) -> SbResult<String> {
    // 1. CLI flag takes highest priority
    if let Some(t) = cli_flag {
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }

    // 2. SB_TOKEN env var (check if token came from env specifically)
    if let Some(ref t) = config.token.value {
        if !t.is_empty() && matches!(config.token.source, ConfigSource::Env(_)) {
            return Ok(t.clone());
        }
    }

    // 3. OS keychain -- only when auth.keychain = true and server_url is known
    if config.auth_keychain.value {
        if let Some(ref server_url) = config.server_url.value {
            match crate::keychain::get_token(server_url) {
                Ok(Some(token)) if !token.is_empty() => return Ok(token),
                Ok(_) => {} // No entry or empty -- fall through to file sources
                Err(e) => {
                    tracing::debug!("keychain lookup failed, falling through: {e}");
                }
            }
        }
    }

    // 4. Per-space config file OR XDG user config file. The resolver in
    // load_from already picked the higher-priority of the two for us.
    if let Some(ref t) = config.token.value {
        if !t.is_empty()
            && matches!(
                config.token.source,
                ConfigSource::File | ConfigSource::UserFile
            )
        {
            return Ok(t.clone());
        }
    }

    // 5. All sources exhausted
    let mut checked = vec![
        "--token flag".into(),
        "SB_TOKEN environment variable".into(),
    ];
    if config.auth_keychain.value {
        checked.push("OS keychain".into());
    }
    checked.push(".sb/config.toml token field".into());
    checked.push("~/.config/sb/config.toml token field".into());

    Err(SbError::TokenNotFound { checked })
}

/// Update a single key in `.sb/config.toml` without destroying existing content.
///
/// Uses `toml_edit` to parse-modify-write atomically:
/// - Creates the `[table]` section if absent
/// - Sets `key = value` within that section
/// - Preserves all existing comments, formatting, and other fields
pub fn update_config_value(
    sb_dir: &Path,
    table: &str,
    key: &str,
    value: impl Into<toml_edit::Value>,
) -> SbResult<()> {
    let config_path = sb_dir.join("config.toml");

    // Read existing content (or start with empty document)
    let content = if config_path.is_file() {
        std::fs::read_to_string(&config_path).map_err(|e| SbError::Filesystem {
            message: format!("cannot read config file: {e}"),
            path: config_path.display().to_string(),
            source: Some(e),
        })?
    } else {
        String::new()
    };

    let mut doc = toml_edit::DocumentMut::from_str(&content).map_err(|e| SbError::Config {
        message: format!("cannot parse config.toml for update: {e}"),
    })?;

    // Create the table section if it does not exist
    if !doc.contains_table(table) {
        doc[table] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    doc[table][key] = toml_edit::Item::Value(value.into());

    std::fs::write(&config_path, doc.to_string()).map_err(|e| SbError::Filesystem {
        message: "failed to write config file".into(),
        path: config_path.display().to_string(),
        source: Some(e),
    })?;

    Ok(())
}

/// Return the XDG config directory for sb: `$XDG_CONFIG_HOME/sb` or `~/.config/sb`.
pub fn xdg_config_dir() -> SbResult<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").map_err(|_| SbError::Config {
            message: "cannot determine home directory: $HOME is not set".to_string(),
        })?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("sb"))
}

/// Expand a leading `~/` or bare `~` against `$HOME`.
/// Absolute paths are returned as-is.
/// Other relative paths return `SbError::Config`.
pub fn expand_tilde(path: &str) -> SbResult<PathBuf> {
    if path == "~" {
        let home = std::env::var("HOME").map_err(|_| SbError::Config {
            message: "cannot expand ~: $HOME is not set".to_string(),
        })?;
        return Ok(PathBuf::from(home));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").map_err(|_| SbError::Config {
            message: "cannot expand ~: $HOME is not set".to_string(),
        })?;
        return Ok(PathBuf::from(home).join(rest));
    }
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return Ok(p);
    }
    Err(SbError::Config {
        message: format!(
            "space path must be absolute or start with ~/: got \"{}\"",
            path
        ),
    })
}

/// Raw structure for the user-level XDG config at `~/.config/sb/config.toml`.
///
/// Mirrors `ConfigFile` plus a `space` pointer. Any field set here is used as a
/// fallback when the per-space `.sb/config.toml` doesn't define it. Per-space
/// values always override these.
#[derive(Debug, Default, serde::Deserialize)]
pub struct UserConfig {
    pub space: Option<String>,
    pub server_url: Option<String>,
    pub token: Option<String>,
    #[serde(default)]
    pub sync: SyncConfigFile,
    #[serde(default)]
    pub daily: DailyConfigFile,
    #[serde(default)]
    pub shell: ShellConfigFile,
    #[serde(default)]
    pub auth: AuthConfigFile,
    #[serde(default)]
    pub runtime: RuntimeConfigFile,
}

/// Load the user-level XDG config. Returns defaults if the file does not exist.
pub fn load_user_config() -> SbResult<UserConfig> {
    let config_path = xdg_config_dir()?.join("config.toml");
    if !config_path.is_file() {
        return Ok(UserConfig::default());
    }
    let content = std::fs::read_to_string(&config_path).map_err(|e| SbError::Config {
        message: format!("cannot read {}: {e}", config_path.display()),
    })?;
    toml::from_str::<UserConfig>(&content).map_err(|e| SbError::Config {
        message: format!("invalid XDG config at {}: {e}", config_path.display()),
    })
}

/// Write the `space` key in `$XDG_CONFIG_HOME/sb/config.toml`.
/// Creates the directory and file if they do not exist.
/// Preserves all other keys via toml_edit.
pub fn write_user_config_space(space_path: &Path) -> SbResult<()> {
    let dir = xdg_config_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| SbError::Filesystem {
        message: "failed to create XDG config directory".into(),
        path: dir.display().to_string(),
        source: Some(e),
    })?;
    let config_path = dir.join("config.toml");
    let content = if config_path.is_file() {
        std::fs::read_to_string(&config_path).map_err(|e| SbError::Filesystem {
            message: format!("cannot read {}", config_path.display()),
            path: config_path.display().to_string(),
            source: Some(e),
        })?
    } else {
        String::new()
    };
    let mut doc = toml_edit::DocumentMut::from_str(&content).map_err(|e| SbError::Config {
        message: format!("cannot parse XDG config.toml: {e}"),
    })?;
    doc["space"] = toml_edit::value(space_path.display().to_string());
    std::fs::write(&config_path, doc.to_string()).map_err(|e| SbError::Filesystem {
        message: "failed to write XDG config file".into(),
        path: config_path.display().to_string(),
        source: Some(e),
    })?;
    Ok(())
}

/// Write an initial `.sb/config.toml` file with the given server URL and optional token.
///
/// - URL trailing slash is stripped before storing (token only written when explicitly
///   provided via CLI flag; env-sourced tokens should not be persisted).
/// - Uses `toml::to_string_pretty` for human-readable output.
pub fn write_config_file(sb_dir: &std::path::Path, url: &str, token: Option<&str>) -> SbResult<()> {
    #[derive(serde::Serialize)]
    struct InitConfig {
        server_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    }

    let cfg = InitConfig {
        server_url: url.trim_end_matches('/').to_string(),
        token: token.map(|t| t.to_string()),
    };

    let content = toml::to_string_pretty(&cfg).map_err(|e| SbError::Config {
        message: format!("failed to serialize config: {e}"),
    })?;

    let config_path = sb_dir.join("config.toml");
    std::fs::write(&config_path, content).map_err(|e| SbError::Filesystem {
        message: "failed to write config file".into(),
        path: config_path.display().to_string(),
        source: Some(e),
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Mutex to serialize tests that modify process-global env vars.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Holds the env mutex AND points `XDG_CONFIG_HOME` at a guaranteed-empty
    /// temp dir for the duration of the test. Restores both on drop.
    ///
    /// `load_from` now consults the XDG user config as a fallback layer.
    /// Without isolation, tests would silently read the developer's real
    /// `~/.config/sb/config.toml` and produce nondeterministic results.
    struct EnvTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _xdg_tmp: tempfile::TempDir,
        prev_xdg: Option<String>,
    }

    impl EnvTestGuard {
        fn new() -> Self {
            let lock = match ENV_MUTEX.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
            let xdg_tmp = tempfile::tempdir().expect("create xdg isolate dir");
            std::env::set_var("XDG_CONFIG_HOME", xdg_tmp.path());
            EnvTestGuard {
                _lock: lock,
                _xdg_tmp: xdg_tmp,
                prev_xdg,
            }
        }
    }

    impl Drop for EnvTestGuard {
        fn drop(&mut self) {
            match &self.prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    /// Helper to create a temp dir with `.sb/config.toml` and return the temp dir.
    fn setup_config_dir(toml_content: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
        let mut f = std::fs::File::create(sb_dir.join("config.toml")).expect("create config.toml");
        f.write_all(toml_content.as_bytes())
            .expect("write config.toml");
        dir
    }

    #[test]
    fn defaults_returns_correct_values() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        // No .sb/config.toml, no env vars
        let config = ResolvedConfig::load_from(dir.path()).expect("load defaults");

        assert_eq!(config.sync_workers.value, 4);
        assert_eq!(config.sync_workers.source, ConfigSource::Default);

        assert!(!config.sync_attachments.value);
        assert_eq!(config.sync_attachments.source, ConfigSource::Default);

        assert_eq!(config.sync_exclude.value, vec!["_plug/*"]);
        assert_eq!(config.sync_exclude.source, ConfigSource::Default);

        assert!(config.sync_include.value.is_empty());
        assert_eq!(config.sync_include.source, ConfigSource::Default);

        assert_eq!(config.daily_path.value, "Journal/{{date}}");
        assert_eq!(config.daily_path.source, ConfigSource::Default);

        assert_eq!(config.daily_date_format.value, "%Y-%m-%d");
        assert_eq!(config.daily_date_format.source, ConfigSource::Default);

        assert!(!config.shell_enabled.value);
        assert_eq!(config.shell_enabled.source, ConfigSource::Default);

        assert!(!config.auth_keychain.value);
        assert_eq!(config.auth_keychain.source, ConfigSource::Default);
    }

    #[test]
    fn loads_values_from_toml_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
server_url = "https://sb.example.com"
token = "secret123"
"#,
        );

        let config = ResolvedConfig::load_from(dir.path()).expect("load config");

        assert_eq!(
            config.server_url.value.as_deref(),
            Some("https://sb.example.com")
        );
        assert_eq!(config.server_url.source, ConfigSource::File);

        assert_eq!(config.token.value.as_deref(), Some("secret123"));
        assert_eq!(config.token.source, ConfigSource::File);
    }

    #[test]
    fn env_var_overrides_file_value_for_token() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
token = "filetoken"
"#,
        );

        std::env::set_var("SB_TOKEN", "envtoken");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_TOKEN");

        assert_eq!(config.token.value.as_deref(), Some("envtoken"));
        assert_eq!(
            config.token.source,
            ConfigSource::Env("SB_TOKEN".to_string())
        );
    }

    #[test]
    fn env_var_overrides_file_value_for_sync_workers() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[sync]
workers = 2
"#,
        );

        std::env::set_var("SB_SYNC_WORKERS", "8");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_SYNC_WORKERS");

        assert_eq!(config.sync_workers.value, 8);
        assert_eq!(
            config.sync_workers.source,
            ConfigSource::Env("SB_SYNC_WORKERS".to_string())
        );
    }

    #[test]
    fn unset_values_use_defaults() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");

        assert_eq!(config.server_url.source, ConfigSource::Default);
        assert!(config.server_url.value.is_none());

        assert_eq!(config.token.source, ConfigSource::Default);
        assert!(config.token.value.is_none());
    }

    #[test]
    fn full_precedence_env_over_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[sync]
workers = 2
"#,
        );

        std::env::set_var("SB_SYNC_WORKERS", "8");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_SYNC_WORKERS");

        // Env (8) should win over file (2)
        assert_eq!(config.sync_workers.value, 8);
        assert_eq!(
            config.sync_workers.source,
            ConfigSource::Env("SB_SYNC_WORKERS".to_string())
        );
    }

    #[test]
    fn file_value_used_when_no_env() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[sync]
workers = 2
"#,
        );

        let config = ResolvedConfig::load_from(dir.path()).expect("load config");

        assert_eq!(config.sync_workers.value, 2);
        assert_eq!(config.sync_workers.source, ConfigSource::File);
    }

    #[test]
    fn mask_token_long_string() {
        assert_eq!(mask_token("sk-abc123def456"), "sk-...456");
    }

    #[test]
    fn mask_token_short_string() {
        assert_eq!(mask_token("ab"), "***");
    }

    #[test]
    fn mask_token_empty_string() {
        assert_eq!(mask_token(""), "");
    }

    #[test]
    fn mask_token_exactly_six_chars() {
        assert_eq!(mask_token("abcdef"), "abc...def");
    }

    #[test]
    fn mask_token_handles_multibyte_utf8() {
        // emoji characters are 4 bytes each — byte slicing would panic
        let token = "🔑🔑🔑secret🔑🔑🔑";
        let masked = mask_token(token);
        assert!(masked.contains("..."), "should contain ellipsis");
        assert!(!masked.contains("secret"), "should not reveal middle");

        // 3-char token should show ***
        assert_eq!(mask_token("abc"), "***");

        // empty token
        assert_eq!(mask_token(""), "");
    }

    #[test]
    fn malformed_toml_returns_config_error() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir("this is not valid toml {{{}}}");
        let result = ResolvedConfig::load_from(dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            SbError::Config { message } => {
                assert!(message.contains("invalid config"));
            }
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    #[test]
    fn env_bool_parsing_accepts_true_1_false_0() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir("");

        std::env::set_var("SB_SHELL_ENABLED", "1");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_SHELL_ENABLED");
        assert!(config.shell_enabled.value);

        std::env::set_var("SB_SHELL_ENABLED", "true");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_SHELL_ENABLED");
        assert!(config.shell_enabled.value);

        std::env::set_var("SB_SHELL_ENABLED", "0");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_SHELL_ENABLED");
        assert!(!config.shell_enabled.value);

        std::env::set_var("SB_SHELL_ENABLED", "false");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_SHELL_ENABLED");
        assert!(!config.shell_enabled.value);
    }

    #[test]
    fn env_vec_parsing_comma_separated() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir("");

        std::env::set_var("SB_SYNC_EXCLUDE", "_plug/*,private/*,drafts/*");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_SYNC_EXCLUDE");

        assert_eq!(
            config.sync_exclude.value,
            vec!["_plug/*", "private/*", "drafts/*"]
        );
        assert_eq!(
            config.sync_exclude.source,
            ConfigSource::Env("SB_SYNC_EXCLUDE".to_string())
        );
    }

    #[test]
    fn config_source_display() {
        assert_eq!(ConfigSource::Default.to_string(), "default");
        assert_eq!(ConfigSource::File.to_string(), "config");
        assert_eq!(
            ConfigSource::Env("SB_TOKEN".to_string()).to_string(),
            "env: SB_TOKEN"
        );
        assert_eq!(
            ConfigSource::Flag("--workers".to_string()).to_string(),
            "flag: --workers"
        );
    }

    #[test]
    fn finds_config_in_parent_directory() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
        std::fs::write(
            sb_dir.join("config.toml"),
            r#"server_url = "https://parent.example.com""#,
        )
        .expect("write config");

        // Create a subdirectory
        let sub_dir = dir.path().join("subdir").join("deep");
        std::fs::create_dir_all(&sub_dir).expect("create subdir");

        let config = ResolvedConfig::load_from(&sub_dir).expect("load from subdir");
        assert_eq!(
            config.server_url.value.as_deref(),
            Some("https://parent.example.com")
        );
        assert_eq!(config.server_url.source, ConfigSource::File);
    }

    // --- runtime config tests ---

    #[test]
    fn runtime_available_defaults_to_false() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = ResolvedConfig::load_from(dir.path()).expect("load defaults");
        assert!(!config.runtime_available.value);
        assert_eq!(config.runtime_available.source, ConfigSource::Default);
    }

    #[test]
    fn runtime_available_loads_true_from_config_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[runtime]
available = true
"#,
        );
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert!(config.runtime_available.value);
        assert_eq!(config.runtime_available.source, ConfigSource::File);
    }

    #[test]
    fn runtime_available_env_var_overrides_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[runtime]
available = false
"#,
        );
        std::env::set_var("SB_RUNTIME_AVAILABLE", "true");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_RUNTIME_AVAILABLE");
        assert!(config.runtime_available.value);
        assert_eq!(
            config.runtime_available.source,
            ConfigSource::Env("SB_RUNTIME_AVAILABLE".to_string())
        );
    }

    #[test]
    fn update_config_value_creates_runtime_section_and_sets_available() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
        // Write an initial config without [runtime]
        std::fs::write(
            sb_dir.join("config.toml"),
            "server_url = \"https://example.com\"\n",
        )
        .expect("write initial config");

        update_config_value(&sb_dir, "runtime", "available", true)
            .expect("update_config_value should succeed");

        let content =
            std::fs::read_to_string(sb_dir.join("config.toml")).expect("read config.toml");
        assert!(
            content.contains("[runtime]"),
            "should contain [runtime] section"
        );
        assert!(
            content.contains("available = true"),
            "should contain available = true"
        );
        // Existing content must be preserved
        assert!(content.contains("server_url"), "should preserve server_url");
        assert!(
            content.contains("https://example.com"),
            "should preserve server URL value"
        );
    }

    #[test]
    fn update_config_value_updates_existing_runtime_section() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
        std::fs::write(sb_dir.join("config.toml"), "[runtime]\navailable = false\n")
            .expect("write initial config");

        update_config_value(&sb_dir, "runtime", "available", true)
            .expect("update_config_value should succeed");

        let content =
            std::fs::read_to_string(sb_dir.join("config.toml")).expect("read config.toml");
        assert!(
            content.contains("available = true"),
            "value should be updated to true"
        );
    }

    #[test]
    fn daily_time_format_defaults_to_hh_mm() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(config.daily_time_format.value, "%H:%M");
        assert_eq!(config.daily_time_format.source, ConfigSource::Default);
    }

    #[test]
    fn daily_time_format_loads_from_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[daily]
timeFormat = "%H:%M:%S"
"#,
        );
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(config.daily_time_format.value, "%H:%M:%S");
        assert_eq!(config.daily_time_format.source, ConfigSource::File);
    }

    #[test]
    fn daily_time_format_env_var_overrides_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[daily]
timeFormat = "%H:%M"
"#,
        );
        std::env::set_var("SB_DAILY_TIME_FORMAT", "%I:%M %p");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_DAILY_TIME_FORMAT");
        assert_eq!(config.daily_time_format.value, "%I:%M %p");
        assert_eq!(
            config.daily_time_format.source,
            ConfigSource::Env("SB_DAILY_TIME_FORMAT".to_string())
        );
    }

    #[test]
    fn daily_bullet_style_defaults_to_star() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(config.daily_bullet_style.value, "*");
        assert_eq!(config.daily_bullet_style.source, ConfigSource::Default);
    }

    #[test]
    fn daily_bullet_style_dash_is_accepted() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[daily]
bulletStyle = "-"
"#,
        );
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(config.daily_bullet_style.value, "-");
    }

    #[test]
    fn daily_bullet_style_invalid_value_errors() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[daily]
bulletStyle = "+"
"#,
        );
        let err = ResolvedConfig::load_from(dir.path()).unwrap_err();
        match err {
            SbError::Config { message } => {
                assert!(
                    message.contains("bulletStyle"),
                    "expected message to mention bulletStyle, got: {message}"
                );
            }
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    #[test]
    fn daily_task_tag_defaults() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(config.daily_task_tag.value, "task");
        assert_eq!(config.daily_task_tag.source, ConfigSource::Default);
        assert_eq!(config.daily_task_tag_mode.value, "auto");
        assert_eq!(config.daily_task_tag_mode.source, ConfigSource::Default);
    }

    #[test]
    fn daily_task_tag_loads_from_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[daily]
taskTag = "todo"
taskTagMode = "always"
"#,
        );
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(config.daily_task_tag.value, "todo");
        assert_eq!(config.daily_task_tag.source, ConfigSource::File);
        assert_eq!(config.daily_task_tag_mode.value, "always");
    }

    #[test]
    fn daily_task_tag_mode_invalid_value_errors() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[daily]
taskTagMode = "sometimes"
"#,
        );
        let err = ResolvedConfig::load_from(dir.path()).unwrap_err();
        match err {
            SbError::Config { message } => {
                assert!(
                    message.contains("taskTagMode"),
                    "expected message to mention taskTagMode, got: {message}"
                );
            }
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    #[test]
    fn daily_template_loads_from_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
[daily]
template = "Daily note for {{date}}"
"#,
        );

        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(
            config.daily_template.value.as_deref(),
            Some("Daily note for {{date}}")
        );
        assert_eq!(config.daily_template.source, ConfigSource::File);
    }

    // --- resolve_token tests ---

    /// Helper: build a ResolvedConfig with a specific token value and source.
    fn config_with_token(token: Option<&str>) -> ResolvedConfig {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        match token {
            Some(t) => {
                let toml = format!("token = \"{t}\"");
                let dir2 = setup_config_dir(&toml);
                ResolvedConfig::load_from(dir2.path()).expect("load config")
            }
            None => ResolvedConfig::load_from(dir.path()).expect("load config"),
        }
    }

    #[test]
    fn resolve_token_cli_flag_wins_over_config() {
        let config = config_with_token(Some("config-token"));
        let result = resolve_token(Some("flag-token"), &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "flag-token");
    }

    #[test]
    fn resolve_token_env_wins_over_config_file() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(r#"token = "config-token""#);
        std::env::set_var("SB_TOKEN", "env-token");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_TOKEN");
        let result = resolve_token(None, &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "env-token");
    }

    #[test]
    fn resolve_token_uses_config_file_when_no_flag_or_env() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(r#"token = "config-token""#);
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        let result = resolve_token(None, &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "config-token");
    }

    #[test]
    fn resolve_token_returns_token_not_found_when_all_sources_empty() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        let result = resolve_token(None, &config);
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::SbError::TokenNotFound { checked } => {
                assert!(checked.iter().any(|s| s.contains("--token flag")));
                assert!(checked.iter().any(|s| s.contains("SB_TOKEN")));
                assert!(checked.iter().any(|s| s.contains("config.toml")));
            }
            other => panic!("expected TokenNotFound, got: {other:?}"),
        }
    }

    // --- write_config_file tests ---

    #[test]
    fn write_config_file_creates_config_toml_with_url_and_token() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");

        write_config_file(&sb_dir, "https://example.com", Some("mytoken"))
            .expect("write_config_file should succeed");

        let content =
            std::fs::read_to_string(sb_dir.join("config.toml")).expect("read config.toml");
        assert!(
            content.contains("server_url"),
            "should contain server_url key"
        );
        assert!(content.contains("https://example.com"));
        assert!(content.contains("token"));
        assert!(content.contains("mytoken"));
    }

    #[test]
    fn write_config_file_without_token_omits_token_field() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");

        write_config_file(&sb_dir, "https://example.com", None)
            .expect("write_config_file should succeed");

        let content =
            std::fs::read_to_string(sb_dir.join("config.toml")).expect("read config.toml");
        assert!(content.contains("server_url"));
        assert!(
            !content.contains("token"),
            "token field should be absent when None"
        );
    }

    #[test]
    fn write_config_file_normalizes_trailing_slash_from_url() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");

        write_config_file(&sb_dir, "https://example.com/", None)
            .expect("write_config_file should succeed");

        let content =
            std::fs::read_to_string(sb_dir.join("config.toml")).expect("read config.toml");
        assert!(
            content.contains("https://example.com\""),
            "trailing slash should be stripped; got: {content}"
        );
        assert!(
            !content.contains("https://example.com/\""),
            "trailing slash should not appear in stored URL"
        );
    }

    #[test]
    fn write_config_file_roundtrip_parsed_by_resolved_config() {
        let _g = EnvTestGuard::new();
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");

        write_config_file(&sb_dir, "https://sb.example.com", Some("roundtrip-token"))
            .expect("write_config_file should succeed");

        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(
            config.server_url.value.as_deref(),
            Some("https://sb.example.com")
        );
        assert_eq!(config.token.value.as_deref(), Some("roundtrip-token"));
    }

    // --- keychain-aware resolve_token tests ---

    #[test]
    fn resolve_token_with_keychain_disabled_skips_keychain() {
        // auth_keychain = false -> keychain never consulted; file token still works
        let config = config_with_token(Some("config-token"));
        assert!(!config.auth_keychain.value);
        let result = resolve_token(None, &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "config-token");
    }

    #[test]
    fn resolve_token_env_wins_over_keychain() {
        // Even with auth_keychain = true, env token should win at step 2
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
token = "file-token"
[auth]
keychain = true
"#,
        );
        std::env::set_var("SB_TOKEN", "env-token");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        std::env::remove_var("SB_TOKEN");
        let result = resolve_token(None, &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "env-token");
    }

    #[test]
    fn resolve_token_not_found_includes_keychain_when_enabled() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(
            r#"
server_url = "https://example.com"
[auth]
keychain = true
"#,
        );
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        let result = resolve_token(None, &config);
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::TokenNotFound { checked } => {
                assert!(
                    checked.iter().any(|s| s.contains("OS keychain")),
                    "checked list should include OS keychain when auth.keychain=true, got: {checked:?}"
                );
            }
            other => panic!("expected TokenNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn resolve_token_not_found_omits_keychain_when_disabled() {
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(r#"server_url = "https://example.com""#);
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        let result = resolve_token(None, &config);
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::TokenNotFound { checked } => {
                assert!(
                    !checked.iter().any(|s| s.contains("OS keychain")),
                    "checked list should NOT include OS keychain when auth.keychain=false, got: {checked:?}"
                );
            }
            other => panic!("expected TokenNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn resolve_token_file_token_used_when_keychain_disabled() {
        // With keychain disabled, file token should still work (step 4 fallback)
        let _g = EnvTestGuard::new();
        let dir = setup_config_dir(r#"token = "file-token""#);
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        assert_eq!(config.token.source, ConfigSource::File);
        let result = resolve_token(None, &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "file-token");
    }

    // --- expand_tilde tests ---

    #[test]
    fn expand_tilde_absolute_path_is_unchanged() {
        let result = expand_tilde("/tmp/notes").expect("expand absolute");
        assert_eq!(result, PathBuf::from("/tmp/notes"));
    }

    #[test]
    fn expand_tilde_bare_tilde_expands_to_home() {
        let _g = EnvTestGuard::new();
        std::env::set_var("HOME", "/home/testuser");
        let result = expand_tilde("~").expect("expand ~");
        std::env::remove_var("HOME");
        assert_eq!(result, PathBuf::from("/home/testuser"));
    }

    #[test]
    fn expand_tilde_tilde_slash_expands_to_home_subdir() {
        let _g = EnvTestGuard::new();
        std::env::set_var("HOME", "/home/testuser");
        let result = expand_tilde("~/notes").expect("expand ~/notes");
        std::env::remove_var("HOME");
        assert_eq!(result, PathBuf::from("/home/testuser/notes"));
    }

    #[test]
    fn expand_tilde_relative_path_returns_error() {
        let result = expand_tilde("relative/path");
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::Config { message } => {
                assert!(
                    message.contains("absolute or start with ~/"),
                    "got: {message}"
                );
            }
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    // --- xdg_config_dir tests ---

    #[test]
    fn xdg_config_dir_uses_xdg_config_home_when_set() {
        let _g = EnvTestGuard::new();
        std::env::set_var("XDG_CONFIG_HOME", "/custom/config");
        let result = xdg_config_dir().expect("xdg_config_dir");
        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(result, PathBuf::from("/custom/config/sb"));
    }

    #[test]
    fn xdg_config_dir_defaults_to_home_dot_config_sb() {
        let _g = EnvTestGuard::new();
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", "/home/testuser");
        let result = xdg_config_dir().expect("xdg_config_dir");
        std::env::remove_var("HOME");
        assert_eq!(result, PathBuf::from("/home/testuser/.config/sb"));
    }

    // --- load_user_config / write_user_config_space round-trip ---

    #[test]
    fn user_config_roundtrip_write_then_load() {
        let _g = EnvTestGuard::new();
        let xdg_dir = tempfile::tempdir().expect("create tempdir");
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());

        let space_path = PathBuf::from("/tmp/my-space");
        write_user_config_space(&space_path).expect("write user config");

        let loaded = load_user_config().expect("load user config");
        assert_eq!(loaded.space.as_deref(), Some("/tmp/my-space"));

        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn load_user_config_returns_defaults_when_no_file() {
        let _g = EnvTestGuard::new();
        let xdg_dir = tempfile::tempdir().expect("create tempdir");
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());

        let loaded = load_user_config().expect("load user config");
        assert!(loaded.space.is_none());

        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn write_user_config_space_preserves_existing_keys() {
        let _g = EnvTestGuard::new();
        let xdg_dir = tempfile::tempdir().expect("create tempdir");
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());

        // Write an existing XDG config with an unrelated key
        let config_file = xdg_dir.path().join("sb").join("config.toml");
        std::fs::create_dir_all(config_file.parent().unwrap()).expect("create dir");
        std::fs::write(&config_file, "other_key = \"value\"\n").expect("write initial");

        write_user_config_space(Path::new("/tmp/space")).expect("write user config");

        let content = std::fs::read_to_string(&config_file).expect("read config");
        assert!(content.contains("other_key"), "should preserve other_key");
        assert!(content.contains("/tmp/space"), "should contain space path");

        std::env::remove_var("XDG_CONFIG_HOME");
    }

    // ------------------------------------------------------------------------
    // XDG fallback: load_from layers per-space > XDG > default for every field
    //
    // These tests bypass `EnvTestGuard`'s XDG isolation because they need to
    // populate the XDG file. They take the bare `ENV_MUTEX` directly so they
    // serialize against other env-mutating tests.
    // ------------------------------------------------------------------------

    /// Set up an XDG config dir with the given contents.
    /// Returns (TempDir, prev XDG_CONFIG_HOME) for caller-managed cleanup.
    fn setup_xdg_dir(toml_content: &str) -> (tempfile::TempDir, Option<String>) {
        let xdg_tmp = tempfile::tempdir().expect("create xdg tempdir");
        let sb_subdir = xdg_tmp.path().join("sb");
        std::fs::create_dir_all(&sb_subdir).expect("create xdg/sb dir");
        std::fs::write(sb_subdir.join("config.toml"), toml_content).expect("write xdg config");
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", xdg_tmp.path());
        (xdg_tmp, prev)
    }

    fn restore_xdg(prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    #[test]
    fn xdg_value_used_when_per_space_does_not_define_it() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let space_dir = tempfile::tempdir().expect("create space tempdir");
        let (xdg_tmp, prev) = setup_xdg_dir(
            r#"
server_url = "https://from-xdg.example.com"
[sync]
dir = "from-xdg-sync-dir"
"#,
        );
        // No per-space config file -- XDG should be used as fallback.
        let config = ResolvedConfig::load_from(space_dir.path()).expect("load config");
        assert_eq!(
            config.server_url.value.as_deref(),
            Some("https://from-xdg.example.com")
        );
        assert_eq!(config.server_url.source, ConfigSource::UserFile);
        assert_eq!(config.sync_dir.value, "from-xdg-sync-dir");
        assert_eq!(config.sync_dir.source, ConfigSource::UserFile);
        drop(xdg_tmp);
        restore_xdg(prev);
    }

    #[test]
    fn per_space_overrides_xdg_when_both_define_a_field() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let space_dir = setup_config_dir(r#"server_url = "https://from-per-space.example.com""#);
        let (xdg_tmp, prev) = setup_xdg_dir(r#"server_url = "https://from-xdg.example.com""#);
        let config = ResolvedConfig::load_from(space_dir.path()).expect("load config");
        assert_eq!(
            config.server_url.value.as_deref(),
            Some("https://from-per-space.example.com")
        );
        assert_eq!(
            config.server_url.source,
            ConfigSource::File,
            "per-space file must take precedence over XDG"
        );
        drop(xdg_tmp);
        restore_xdg(prev);
    }

    #[test]
    fn env_overrides_both_per_space_and_xdg() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let space_dir = setup_config_dir(r#"server_url = "https://from-per-space.example.com""#);
        let (xdg_tmp, prev) = setup_xdg_dir(r#"server_url = "https://from-xdg.example.com""#);
        std::env::set_var("SB_SERVER_URL", "https://from-env.example.com");
        let config = ResolvedConfig::load_from(space_dir.path()).expect("load config");
        std::env::remove_var("SB_SERVER_URL");
        assert_eq!(
            config.server_url.value.as_deref(),
            Some("https://from-env.example.com")
        );
        assert!(matches!(config.server_url.source, ConfigSource::Env(_)));
        drop(xdg_tmp);
        restore_xdg(prev);
    }

    #[test]
    fn xdg_token_used_when_per_space_has_no_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let space_dir = setup_config_dir(r#"server_url = "https://example.com""#);
        let (xdg_tmp, prev) = setup_xdg_dir(r#"token = "from-xdg-token""#);
        let config = ResolvedConfig::load_from(space_dir.path()).expect("load config");
        let resolved = resolve_token(None, &config).expect("token must resolve");
        assert_eq!(resolved, "from-xdg-token");
        assert_eq!(config.token.source, ConfigSource::UserFile);
        drop(xdg_tmp);
        restore_xdg(prev);
    }

    #[test]
    fn xdg_daily_and_runtime_blocks_are_honored() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let space_dir = tempfile::tempdir().expect("create space tempdir");
        let (xdg_tmp, prev) = setup_xdg_dir(
            r#"
[daily]
path = "Journals/{{date}}"
[runtime]
available = true
[shell]
enabled = true
"#,
        );
        let config = ResolvedConfig::load_from(space_dir.path()).expect("load config");
        assert_eq!(config.daily_path.value, "Journals/{{date}}");
        assert_eq!(config.daily_path.source, ConfigSource::UserFile);
        assert!(config.runtime_available.value);
        assert_eq!(config.runtime_available.source, ConfigSource::UserFile);
        assert!(config.shell_enabled.value);
        assert_eq!(config.shell_enabled.source, ConfigSource::UserFile);
        drop(xdg_tmp);
        restore_xdg(prev);
    }

    #[test]
    fn token_not_found_message_lists_xdg_source() {
        let _g = EnvTestGuard::new();
        // No per-space token, no XDG file (EnvTestGuard isolates XDG to an empty dir).
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = ResolvedConfig::load_from(dir.path()).expect("load config");
        let err = resolve_token(None, &config).unwrap_err();
        match err {
            SbError::TokenNotFound { checked } => {
                assert!(
                    checked.iter().any(|s| s.contains(".config/sb/config.toml")),
                    "checked list should mention XDG config; got: {checked:?}"
                );
            }
            other => panic!("expected TokenNotFound, got: {other:?}"),
        }
    }
}
