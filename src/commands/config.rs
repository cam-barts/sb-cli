use crate::cli::OutputFormat;
use crate::config::{mask_token, ConfigSource, ResolvedConfig, ResolvedValue};
use crate::error::{SbError, SbResult};
use crate::output;
use tracing::debug;

/// Execute `sb config show` — display resolved configuration with source annotations.
pub fn execute_show(reveal: bool, format: &OutputFormat, quiet: bool) -> SbResult<()> {
    debug!("executing config show (reveal={reveal}, quiet={quiet})");

    if quiet {
        return Ok(());
    }

    let config = match crate::commands::page::find_space_root() {
        Ok(space_root) => ResolvedConfig::load_from(&space_root)?,
        Err(_) => ResolvedConfig::load()?,
    };
    debug!("config loaded successfully");

    match format {
        OutputFormat::Json => print_json(&config, reveal),
        OutputFormat::Human => print_human(&config, reveal),
    }

    Ok(())
}

/// Execute `sb config set-space <path>` — write the default space to XDG config.
pub fn execute_set_space(path: &str, quiet: bool, color: bool) -> SbResult<()> {
    let expanded = crate::config::expand_tilde(path)?;
    if !expanded.join(".sb").join("config.toml").is_file() {
        return Err(SbError::Usage(format!(
            "no .sb/ directory found at {}; run `sb init <url>` there first",
            expanded.display()
        )));
    }
    crate::config::write_user_config_space(&expanded)?;
    output::print_success(
        &format!("Space set to {}", expanded.display()),
        color,
        quiet,
    );
    Ok(())
}

/// Execute `sb config get-space` — show the resolved space root and where it came from.
pub fn execute_get_space(format: &OutputFormat, quiet: bool) -> SbResult<()> {
    if quiet {
        return Ok(());
    }

    // 1. SB_SPACE env
    if let Ok(val) = std::env::var("SB_SPACE") {
        let expanded = crate::config::expand_tilde(&val)?;
        print_space_result(format, &expanded.display().to_string(), "env: SB_SPACE");
        return Ok(());
    }

    // 2. cwd walk-up
    let cwd = std::env::current_dir().map_err(|e| SbError::Config {
        message: format!("cannot determine current directory: {e}"),
    })?;
    if let Ok(root) = crate::commands::page::find_space_root_from(&cwd) {
        print_space_result(format, &root.display().to_string(), "cwd (walk-up)");
        return Ok(());
    }

    // 3. XDG config
    let user_config = crate::config::load_user_config()?;
    if let Some(ref path_str) = user_config.space {
        let expanded = crate::config::expand_tilde(path_str)?;
        let xdg_path = crate::config::xdg_config_dir()
            .map(|d| d.join("config.toml").display().to_string())
            .unwrap_or_else(|_| "~/.config/sb/config.toml".to_string());
        print_space_result(format, &expanded.display().to_string(), &format!("XDG config ({xdg_path})"));
        return Ok(());
    }

    // 4. Not configured
    match format {
        OutputFormat::Json => println!("{}", serde_json::json!({"space": null, "source": "not configured"})),
        OutputFormat::Human => println!("(not configured)"),
    }
    Ok(())
}

fn print_space_result(format: &OutputFormat, space: &str, source: &str) {
    match format {
        OutputFormat::Json => println!("{}", serde_json::json!({"space": space, "source": source})),
        OutputFormat::Human => println!("{space}  # ({source})"),
    }
}

fn source_annotation(source: &ConfigSource) -> String {
    format!("# ({})", source)
}

fn format_string_value(val: &Option<String>, is_token: bool, reveal: bool) -> String {
    match val {
        Some(v) => {
            if is_token && !reveal {
                format!("\"{}\"", mask_token(v))
            } else {
                format!("\"{v}\"")
            }
        }
        None => "(not set)".to_string(),
    }
}

fn format_vec_value(val: &[String]) -> String {
    let items: Vec<String> = val.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", items.join(", "))
}

fn print_human(config: &ResolvedConfig, reveal: bool) {
    // [server]
    println!("[server]");
    println!(
        "server_url = {}  {}",
        format_string_value(&config.server_url.value, false, reveal),
        source_annotation(&config.server_url.source)
    );
    println!(
        "token = {}  {}",
        format_string_value(&config.token.value, true, reveal),
        source_annotation(&config.token.source)
    );
    println!();

    // [sync]
    println!("[sync]");
    println!(
        "workers = {}  {}",
        config.sync_workers.value,
        source_annotation(&config.sync_workers.source)
    );
    println!(
        "attachments = {}  {}",
        config.sync_attachments.value,
        source_annotation(&config.sync_attachments.source)
    );
    println!(
        "exclude = {}  {}",
        format_vec_value(&config.sync_exclude.value),
        source_annotation(&config.sync_exclude.source)
    );
    println!(
        "include = {}  {}",
        format_vec_value(&config.sync_include.value),
        source_annotation(&config.sync_include.source)
    );
    println!();

    // [daily]
    println!("[daily]");
    println!(
        "path = \"{}\"  {}",
        config.daily_path.value,
        source_annotation(&config.daily_path.source)
    );
    println!(
        "dateFormat = \"{}\"  {}",
        config.daily_date_format.value,
        source_annotation(&config.daily_date_format.source)
    );
    if let Some(ref tmpl) = config.daily_template.value {
        println!(
            "template = \"{}\"  {}",
            tmpl,
            source_annotation(&config.daily_template.source)
        );
    }
    println!();

    // [shell]
    println!("[shell]");
    println!(
        "enabled = {}  {}",
        config.shell_enabled.value,
        source_annotation(&config.shell_enabled.source)
    );
    println!();

    // [auth]
    println!("[auth]");
    println!(
        "keychain = {}  {}",
        config.auth_keychain.value,
        source_annotation(&config.auth_keychain.source)
    );
}

fn source_to_json_string(source: &ConfigSource) -> String {
    match source {
        ConfigSource::Default => "default".to_string(),
        ConfigSource::File => "config".to_string(),
        ConfigSource::Env(name) => format!("env: {name}"),
        ConfigSource::Flag(name) => format!("flag: {name}"),
    }
}

fn json_entry<T: serde::Serialize>(val: &ResolvedValue<T>) -> serde_json::Value {
    serde_json::json!({
        "value": val.value,
        "source": source_to_json_string(&val.source),
    })
}

fn json_optional_string_entry(
    val: &ResolvedValue<Option<String>>,
    is_token: bool,
    reveal: bool,
) -> serde_json::Value {
    let display_value = match &val.value {
        Some(v) => {
            if is_token && !reveal {
                serde_json::Value::String(mask_token(v))
            } else {
                serde_json::Value::String(v.clone())
            }
        }
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "value": display_value,
        "source": source_to_json_string(&val.source),
    })
}

fn print_json(config: &ResolvedConfig, reveal: bool) {
    let output = serde_json::json!({
        "server": {
            "server_url": json_optional_string_entry(&config.server_url, false, reveal),
            "token": json_optional_string_entry(&config.token, true, reveal),
        },
        "sync": {
            "workers": json_entry(&config.sync_workers),
            "attachments": json_entry(&config.sync_attachments),
            "exclude": json_entry(&config.sync_exclude),
            "include": json_entry(&config.sync_include),
        },
        "daily": {
            "path": json_entry(&config.daily_path),
            "dateFormat": json_entry(&config.daily_date_format),
        },
        "shell": {
            "enabled": json_entry(&config.shell_enabled),
        },
        "auth": {
            "keychain": json_entry(&config.auth_keychain),
        },
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&output).expect("JSON serialization")
    );
}
