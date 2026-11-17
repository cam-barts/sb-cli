//! `sb search`: full-text search through the server's `/.search/query`.
//!
//! A plain HTTP endpoint (SilverBullet 3.0+), so unlike `sb query` it needs no
//! Runtime API. The response looks like:
//!
//! ```json
//! {"items":[{"page":"Garden/Tea","pos":120,"line":7,"heading":"Brewing",
//!            "excerpt":"steep green tea at 80C","highlights":[[12,15]]}],
//!  "total":1,"partial":false,"notice":null}
//! ```

use reqwest::StatusCode;

use crate::cli::{OutputFormat, SearchMode, SearchScope};
use crate::commands::server::build_client;
use crate::error::{SbError, SbResult};

#[allow(clippy::too_many_arguments)]
pub async fn execute(
    cli_token: Option<&str>,
    query: &str,
    mode: Option<SearchMode>,
    scope: Option<SearchScope>,
    limit: u16,
    fields: &[String],
    format: &OutputFormat,
    quiet: bool,
) -> SbResult<()> {
    let client = build_client(cli_token)?;
    let url = format!("{}/.search/query", client.base_url());
    // Unset options are left out so the server's own defaults apply.
    let mut body = serde_json::json!({ "query": query, "limit": limit });
    if let Some(mode) = mode {
        body["mode"] = serde_json::json!(mode);
    }
    if let Some(scope) = scope {
        body["scope"] = serde_json::json!(scope);
    }

    let resp = client.post_json("/.search/query", &body).await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    match status {
        s if s.is_success() => {}
        // 404: no search backend for this space. 405: a pre-3.0 server with no
        // route at all, whose catch-all only accepts GET.
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => {
            return Err(SbError::SearchUnavailable)
        }
        StatusCode::BAD_REQUEST => {
            let message = parsed["message"].as_str().unwrap_or(text.trim());
            return Err(SbError::Usage(message.to_string()));
        }
        _ => {
            return Err(SbError::SearchFailed {
                status: status.as_u16(),
                message: parsed["message"].as_str().unwrap_or(&url).to_string(),
            })
        }
    }
    // A 200 that isn't a result (an auth proxy's login page, after reqwest
    // followed its redirect) must not read as "No matches".
    if !parsed["items"].is_array() {
        return Err(SbError::SearchFailed {
            status: status.as_u16(),
            message: format!("{url} did not return search results"),
        });
    }

    if let Some(notice) = parsed["notice"].as_str() {
        if !quiet {
            eprintln!("{notice}");
        }
    }

    let items = &parsed["items"];
    let mut out = std::io::stdout().lock();
    if matches!(format, OutputFormat::Human) && fields.is_empty() {
        render_human(items, quiet, &mut out).map_err(|e| SbError::Internal {
            message: format!("failed to write output: {e}"),
        })
    } else {
        crate::commands::links::render(items, fields, format, quiet, "No matches.", &mut out)
    }
}

/// `page:line  heading`, with the excerpt indented underneath.
fn render_human(
    items: &serde_json::Value,
    quiet: bool,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let hits = items.as_array().map(Vec::as_slice).unwrap_or_default();
    if hits.is_empty() && !quiet {
        eprintln!("No matches.");
    }
    for hit in hits {
        let page = hit["page"].as_str().unwrap_or("");
        let line = hit["line"].as_u64().unwrap_or(0);
        match hit["heading"].as_str() {
            Some(heading) => writeln!(out, "{page}:{line}  {heading}")?,
            None => writeln!(out, "{page}:{line}")?,
        }
        let excerpt = hit["excerpt"].as_str().unwrap_or("").trim();
        writeln!(out, "    {}", excerpt.replace('\n', "\n    "))?;
    }
    Ok(())
}
