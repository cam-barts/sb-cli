use std::path::Path;

use crate::cli::OutputFormat;
use crate::commands::page::{find_space_root, open_in_editor, validate_page_path};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};
use crate::output;

/// Resolve the target date given offset days from today.
///
/// offset=0 means today, offset=-1 means yesterday.
/// --yesterday is converted to offset=-1 by the caller.
pub fn resolve_daily_date(offset: i64) -> SbResult<jiff::civil::Date> {
    let today = jiff::Zoned::now().date();
    if offset == 0 {
        Ok(today)
    } else {
        use jiff::ToSpan;
        today
            .checked_add(offset.days())
            .map_err(|e| SbError::Config {
                message: format!("invalid date offset {}: {}", offset, e),
            })
    }
}

/// Expand `{{date}}` in the daily path template using the given date and format.
pub fn format_daily_path(
    path_template: &str,
    date_format: &str,
    date: &jiff::civil::Date,
) -> String {
    let formatted_date = date.strftime(date_format).to_string();
    path_template.replace("{{date}}", &formatted_date)
}

/// Fetch template content: try local file first, then remote.
///
/// Returns `None` when the template cannot be found in either location.
async fn fetch_template_content(
    space_root: &Path,
    template_name: &str,
    config: &ResolvedConfig,
    cli_token: Option<&str>,
) -> SbResult<Option<String>> {
    // Try local file first
    let local_path = space_root.join(format!("{}.md", template_name));
    if local_path.exists() {
        let content = std::fs::read_to_string(&local_path).map_err(|e| SbError::Filesystem {
            message: "failed to read template".into(),
            path: local_path.display().to_string(),
            source: Some(e),
        })?;
        return Ok(Some(content));
    }

    // Try remote if server is configured
    if let Some(ref url) = config.server_url.value {
        match crate::config::resolve_token(cli_token, config) {
            Ok(token) => {
                let client = crate::client::SbClient::new(url, &token)?;
                match client.get_page(template_name).await {
                    Ok(content) => return Ok(Some(content)),
                    Err(SbError::PageNotFound { .. }) => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
            Err(SbError::TokenNotFound { .. }) => {
                // No token available — skip remote template fetch
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }

    Ok(None)
}

/// Execute the `sb daily` command.
///
/// Daily note management with configurable path templates.
pub async fn execute(
    cli_token: Option<&str>,
    append: Option<&str>,
    yesterday: bool,
    offset: Option<i64>,
    _format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    let space_root = find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;

    // Resolve date: --yesterday = offset -1, --offset N, default = 0 (today)
    let date_offset = if yesterday { -1 } else { offset.unwrap_or(0) };
    let date = resolve_daily_date(date_offset)?;

    // Expand path template
    let page_name = format_daily_path(
        &config.daily_path.value,
        &config.daily_date_format.value,
        &date,
    );

    // Security: validate before any filesystem access
    let page_path = validate_page_path(&space_root, &page_name)?;

    // Create note if it doesn't exist
    let created = if !page_path.exists() {
        // Create parent directories
        if let Some(parent) = page_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
                message: "failed to create directory".into(),
                path: parent.display().to_string(),
                source: Some(e),
            })?;
        }

        // Get template content if configured
        let content = if let Some(ref template_name) = config.daily_template.value {
            fetch_template_content(&space_root, template_name, &config, cli_token)
                .await?
                .unwrap_or_default()
        } else {
            String::new()
        };

        std::fs::write(&page_path, &content).map_err(|e| SbError::Filesystem {
            message: "failed to create daily note".into(),
            path: page_path.display().to_string(),
            source: Some(e),
        })?;

        output::print_success(&format!("Created {}", page_name), color, quiet);
        true
    } else {
        false
    };

    // Handle --append mode: append without opening editor
    if let Some(text) = append {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&page_path)
            .map_err(|e| SbError::Filesystem {
                message: "failed to open daily note for append".into(),
                path: page_path.display().to_string(),
                source: Some(e),
            })?;

        // If note was just created from a template (may have content), add a newline separator.
        // If note already existed, always add a newline separator (consistent with page append).
        let prefix = if created {
            // Check if template wrote any content
            let has_content = std::fs::metadata(&page_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
            if has_content {
                "\n"
            } else {
                ""
            }
        } else {
            "\n"
        };

        file.write_all(format!("{}{}", prefix, text).as_bytes())
            .map_err(|e| SbError::Filesystem {
                message: "failed to append to daily note".into(),
                path: page_path.display().to_string(),
                source: Some(e),
            })?;

        output::print_success(&format!("Appended to {}", page_name), color, quiet);
        return Ok(());
    }

    // Default: open in editor
    open_in_editor(&page_path).await?;
    Ok(())
}
