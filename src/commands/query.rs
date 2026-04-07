use crate::cli::OutputFormat;
use crate::commands::server::{build_client, require_initialized, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

pub async fn execute(
    cli_token: Option<&str>,
    query: &str,
    format: &OutputFormat,
    quiet: bool,
    _color: bool,
) -> SbResult<()> {
    require_initialized()?;
    let config = ResolvedConfig::load()?;

    // Check runtime availability
    if !config.runtime_available.value {
        return Err(runtime_unavailable_error());
    }

    let client = build_client(cli_token)?;

    // Wrap query as Lua script using query[[...]] syntax
    let lua_script = format!("return query[[{}]]", query);
    let resp = client
        .post_text("/.runtime/lua_script", &lua_script)
        .await?;
    let status = resp.status();

    // Handle 503: Runtime API went down between detection and use
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return Err(runtime_unavailable_error());
    }

    let body = resp.text().await.map_err(|e| SbError::HttpStatus {
        status: status.as_u16(),
        url: format!("{}/.runtime/lua_script", client.base_url()),
        body: format!("failed to read response: {e}"),
    })?;

    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url: format!("{}/.runtime/lua_script", client.base_url()),
            body: format!("invalid JSON response: {e}"),
        })?;

    if let Some(error) = parsed.get("error").and_then(|e| e.as_str()) {
        return Err(SbError::HttpStatus {
            status: status.as_u16(),
            url: format!("{}/.runtime/lua_script", client.base_url()),
            body: format!("Query error: {error}"),
        });
    }

    let result = parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match format {
        OutputFormat::Json => {
            // Raw JSON output
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            // Table format for arrays of objects, JSON fallback otherwise
            if let Some(arr) = result.as_array() {
                if arr.is_empty() {
                    if !quiet {
                        eprintln!("No results.");
                    }
                } else {
                    render_table(arr);
                }
            } else {
                // Non-array: fall back to JSON
                if !quiet {
                    eprintln!("note: result is not an array; displaying as JSON");
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
        }
    }

    Ok(())
}

/// Render a JSON array of objects as an ASCII table.
/// Extracts column names from the keys of the first object.
/// Uses simple format!() padding -- no external table crate needed.
fn render_table(rows: &[serde_json::Value]) {
    // Collect column names from first row
    let columns: Vec<String> = if let Some(obj) = rows.first().and_then(|r| r.as_object()) {
        obj.keys().cloned().collect()
    } else {
        // Not objects -- just dump each value on a line
        for row in rows {
            println!("{}", value_to_string(row));
        }
        return;
    };

    // Calculate column widths
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        if let Some(obj) = row.as_object() {
            for (i, col) in columns.iter().enumerate() {
                let val = obj.get(col).map(value_to_string).unwrap_or_default();
                widths[i] = widths[i].max(val.len());
            }
        }
    }

    // Print header
    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
        .collect();
    println!("{}", header.join(" | "));

    // Print separator
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep.join("-+-"));

    // Print rows
    for row in rows {
        if let Some(obj) = row.as_object() {
            let cells: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let val = obj.get(col).map(value_to_string).unwrap_or_default();
                    format!("{:<width$}", val, width = widths[i])
                })
                .collect();
            println!("{}", cells.join(" | "));
        }
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "".to_string(),
        other => other.to_string(),
    }
}
