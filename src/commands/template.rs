//! Page templates as a first-class concept.
//!
//! SilverBullet declares a page template by tagging the page
//! `meta/template/page`; that convention is surfaced through the index
//! (`from index.tag "meta/template/page"`). Such a page carries a nested
//! `frontmatter:` block (the frontmatter for pages created from it), optional
//! `command`/`suggestedName`/`confirmName` metadata, and a body that may contain
//! `${...}` Space Lua expressions and a `|^|` cursor marker. This module reuses
//! that convention two ways:
//!
//! - **Discovery**: when the Runtime API is available we query the index for the
//!   `meta/template/page` tag (authoritative, matches the editor's own notion of
//!   a page template). Offline, we fall back to scanning synced `.md` frontmatter
//!   for that tag so `sb template list`/`new` still work without a server.
//! - **Instantiation**: the template's `frontmatter:` block becomes the new
//!   page's frontmatter and the `|^|` marker is filled (piped stdin) or dropped.
//!   With the Runtime API we then render the result via `template.new` so
//!   `${...}` resolves; otherwise `${...}` is left literal.
//!
//! Naming/behavior frontmatter is honored per SilverBullet's contract:
//! `suggestedName` (rendered) + `confirmName` (default true) resolve the new
//! page's name, `openIfExists` opens an existing target instead of erroring, and
//! `description` is surfaced in `sb template list`. The UI-only fields
//! (`command`, `key`/`mac`, `priority`) have no CLI analog and are ignored.

use std::io::IsTerminal;
use std::path::Path;

use crate::cli::OutputFormat;
use crate::client::SbClient;
use crate::commands::page::{
    find_content_dir, find_space_root, open_in_editor, path_to_page_name, validate_page_path,
};
use crate::commands::server::{build_client, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

/// The tag SilverBullet uses to declare a page template. Pages tagged this way
/// are offered as "new page from template" in the editor; `sb` reuses the same
/// notion. (Other template kinds like `meta/template/snippet` are not page
/// templates and are intentionally excluded from `sb template new`.)
const PAGE_TEMPLATE_TAG: &str = "meta/template/page";

/// Where a template was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSource {
    /// Reported by the SilverBullet index (`from index.tag "meta/template/page"`).
    Index,
    /// Found by scanning local synced frontmatter.
    Local,
}

impl TemplateSource {
    fn label(self) -> &'static str {
        match self {
            TemplateSource::Index => "index",
            TemplateSource::Local => "local",
        }
    }
}

/// A template page.
#[derive(Debug, Clone)]
pub struct TemplateInfo {
    pub name: String,
    pub source: TemplateSource,
    /// The template's `description` frontmatter, when present.
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `sb template list` — list pages tagged as templates.
pub async fn execute_list(
    cli_token: Option<&str>,
    format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    let space_root = find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;
    let templates = discover_templates(cli_token, &config).await?;
    render_list(&templates, format, quiet, color);
    Ok(())
}

/// `sb template new [name]` — create a page from a template. When `template` is
/// `None`, present an interactive picker (fzf when available, numbered prompt
/// otherwise). When `name` is `None`, fall back to the template's
/// `suggestedName`/`confirmName` (SilverBullet's "new page from template" flow).
#[allow(clippy::too_many_arguments)]
pub async fn execute_new(
    cli_token: Option<&str>,
    name: Option<&str>,
    template: Option<&str>,
    no_edit: bool,
    dry_run: bool,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    let space_root = find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;
    let content_dir = find_content_dir()?;

    // Piped stdin (if any) is spliced at the template's `|^|` marker. Reading it
    // first is safe: when stdin is piped there is no TTY for the picker, so a
    // template name must be given explicitly.
    let cursor_fill = read_piped_stdin()?;

    // Resolve which template to use.
    let template_name = match template {
        Some(t) => t.to_string(),
        None => {
            let templates = discover_templates(cli_token, &config).await?;
            match pick_template(&templates).await? {
                Some(n) => n,
                None => {
                    if !quiet {
                        eprintln!("Cancelled — no page created.");
                    }
                    return Ok(());
                }
            }
        }
    };

    // Fetch the template once; used for both name resolution (suggestedName /
    // confirmName) and body instantiation.
    let raw = fetch_template_content(&content_dir, &template_name, &config, cli_token)
        .await?
        .ok_or_else(|| SbError::PageNotFound {
            name: template_name.clone(),
        })?;

    // Name/existence hints from the template frontmatter.
    let hints = parse_name_hints(&raw);

    // Resolve the new page name: an explicit argument wins; otherwise fall back
    // to the template's `suggestedName`/`confirmName`.
    let page_name = match name {
        Some(n) => n.to_string(),
        None => resolve_new_page_name(cli_token, &config, &hints).await?,
    };

    let page_path = validate_page_path(&content_dir, &page_name)?;
    if page_path.exists() {
        // `openIfExists`: don't overwrite — open the existing page instead of
        // erroring (SilverBullet's "navigate to it" behavior).
        if hints.open_if_exists {
            crate::output::print_success(
                &format!("page '{page_name}' already exists; opening"),
                color,
                quiet,
            );
            maybe_open_editor(&page_path, no_edit).await?;
            return Ok(());
        }
        return Err(SbError::PageAlreadyExists { name: page_name });
    }

    // --dry-run previews the resolved target after name/existence checks,
    // without rendering the template or writing anything.
    if dry_run {
        crate::output::print_success(
            &format!("[dry-run] would create page '{page_name}' from template '{template_name}'"),
            color,
            quiet,
        );
        return Ok(());
    }

    let body = render_from_raw(cli_token, &config, &raw, cursor_fill.as_deref()).await?;

    if let Some(parent) = page_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
            message: "failed to create parent directories".to_string(),
            path: parent.display().to_string(),
            source: Some(e),
        })?;
    }
    std::fs::write(&page_path, &body).map_err(|e| SbError::Filesystem {
        message: "failed to write page".to_string(),
        path: page_path.display().to_string(),
        source: Some(e),
    })?;

    crate::output::print_success(
        &format!("created page '{page_name}' from template '{template_name}'"),
        color,
        quiet,
    );

    // Drop into the editor on the freshly rendered page (unless suppressed or
    // non-interactive).
    maybe_open_editor(&page_path, no_edit).await?;
    Ok(())
}

/// Open `path` in `$EDITOR` after creation. Suppressed by `--no-edit` or when
/// not attached to a terminal (so scripts and pipes never block). A missing
/// `$EDITOR` is tolerated — the page is already written.
async fn maybe_open_editor(path: &Path, no_edit: bool) -> SbResult<()> {
    if no_edit || crate::output::no_input() {
        return Ok(());
    }
    match open_in_editor(path).await {
        Ok(()) | Err(SbError::EditorNotSet) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Name/existence hints parsed from a page template's frontmatter.
struct NameHints {
    /// `suggestedName` (may contain `${...}`; not yet rendered).
    suggested: String,
    /// `confirmName` — defaults to true (SilverBullet's default).
    confirm: bool,
    /// `openIfExists` — defaults to false.
    open_if_exists: bool,
}

fn parse_name_hints(raw: &str) -> NameHints {
    let fm = split_frontmatter(raw).map(|(fm, _)| fm).unwrap_or("");
    NameHints {
        suggested: frontmatter_scalar(fm, "suggestedName").unwrap_or_default(),
        confirm: frontmatter_bool(fm, "confirmName").unwrap_or(true),
        open_if_exists: frontmatter_bool(fm, "openIfExists").unwrap_or(false),
    }
}

/// Resolve the target page name from a template's `suggestedName`/`confirmName`
/// hints, mirroring SilverBullet's "new page from template" flow.
///
/// - `suggestedName` is rendered (so `${...}` resolves) and used as the name.
/// - The name is confirmed interactively when `confirmName` is true (SB's
///   default), when the suggestion is empty, ends with `/` (a folder prefix that
///   still needs a leaf), or couldn't be rendered (still contains `${`).
/// - Non-interactive (no TTY) callers that would need to confirm get a usage
///   error asking for an explicit name.
async fn resolve_new_page_name(
    cli_token: Option<&str>,
    config: &ResolvedConfig,
    hints: &NameHints,
) -> SbResult<String> {
    let suggested = hints.suggested.clone();
    let rendered = if !suggested.is_empty() && config.runtime_available.value {
        render_via_runtime(cli_token, &suggested)
            .await
            .unwrap_or(suggested)
    } else {
        suggested
    };

    let needs_confirm =
        hints.confirm || rendered.is_empty() || rendered.ends_with('/') || rendered.contains("${");

    if !needs_confirm {
        return Ok(rendered);
    }

    if crate::output::no_input() {
        return Err(SbError::Usage(format!(
            "template suggests a name that needs confirmation{}; \
             pass an explicit name: sb template new <name> --template <t>",
            if rendered.is_empty() {
                String::new()
            } else {
                format!(" ('{rendered}')")
            }
        )));
    }

    prompt_page_name(&rendered).await
}

/// Interactively confirm/complete a page name. A suggestion ending in `/` is a
/// folder prefix: the user types the leaf, which is appended. Otherwise the
/// suggestion is offered as a default (Enter accepts, or type a replacement).
async fn prompt_page_name(suggested: &str) -> SbResult<String> {
    let suggested = suggested.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut input = String::new();
        if let Some(prefix) = suggested.strip_suffix('/') {
            eprint!("Page name under '{prefix}/': ");
            std::io::stderr().flush().ok();
            std::io::stdin().read_line(&mut input).ok();
            let leaf = input.trim();
            if leaf.is_empty() {
                return Err(SbError::Usage("page name must not be empty".into()));
            }
            Ok(format!("{prefix}/{leaf}"))
        } else if suggested.is_empty() {
            eprint!("Page name: ");
            std::io::stderr().flush().ok();
            std::io::stdin().read_line(&mut input).ok();
            let name = input.trim();
            if name.is_empty() {
                return Err(SbError::Usage("page name must not be empty".into()));
            }
            Ok(name.to_string())
        } else {
            eprint!("Page name [{suggested}]: ");
            std::io::stderr().flush().ok();
            std::io::stdin().read_line(&mut input).ok();
            let name = input.trim();
            Ok(if name.is_empty() {
                suggested
            } else {
                name.to_string()
            })
        }
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("name prompt task failed: {e}"),
    })?
}

fn render_list(templates: &[TemplateInfo], format: &OutputFormat, quiet: bool, _color: bool) {
    match format {
        OutputFormat::Json => {
            let arr: Vec<serde_json::Value> = templates
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "source": t.source.label(),
                        "description": t.description,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Array(arr)).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if templates.is_empty() {
                if !quiet {
                    eprintln!("No templates found (pages tagged `meta/template/page`).");
                }
                return;
            }
            // Pad names into a column so descriptions line up.
            let width = templates.iter().map(|t| t.name.len()).max().unwrap_or(0);
            for t in templates {
                match &t.description {
                    Some(d) if !d.is_empty() => {
                        println!("{:width$}  {}", t.name, d, width = width)
                    }
                    _ => println!("{}", t.name),
                }
            }
            if !quiet {
                eprintln!(
                    "{} template{} found.",
                    templates.len(),
                    if templates.len() == 1 { "" } else { "s" }
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover templates: index-backed when the Runtime API is available, else a
/// local frontmatter scan.
pub async fn discover_templates(
    cli_token: Option<&str>,
    config: &ResolvedConfig,
) -> SbResult<Vec<TemplateInfo>> {
    if config.runtime_available.value {
        discover_via_index(cli_token).await
    } else {
        let content_dir = find_content_dir()?;
        discover_via_scan(&content_dir)
    }
}

async fn discover_via_index(cli_token: Option<&str>) -> SbResult<Vec<TemplateInfo>> {
    let client = build_client(cli_token)?;
    // Query the full tag objects (name, description, …) and extract fields in
    // Rust. A `select { … description = _.description … }` projection triggers a
    // server-side 500 on some SilverBullet builds, so we avoid it.
    let result = eval_lua_result(
        &client,
        r#"return query[[from index.tag "meta/template/page"]]"#,
    )
    .await?;

    let mut out = Vec::new();
    if let Some(arr) = result.as_array() {
        for row in arr {
            if let Some(name) = row_name(row) {
                let description = row
                    .get("description")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                out.push(TemplateInfo {
                    name,
                    source: TemplateSource::Index,
                    description,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
}

/// Extract a page name from an index row, tolerating a few shapes:
/// a bare string, or an object with `name`/`ref`/`page`.
fn row_name(row: &serde_json::Value) -> Option<String> {
    if let Some(s) = row.as_str() {
        return Some(s.to_string());
    }
    for key in ["name", "ref", "page"] {
        if let Some(s) = row.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn discover_via_scan(content_dir: &Path) -> SbResult<Vec<TemplateInfo>> {
    use walkdir::WalkDir;

    let sb_dir = content_dir.join(".sb");
    let mut out = Vec::new();

    for entry in WalkDir::new(content_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !e.path().starts_with(&sb_dir))
    {
        let entry = entry.map_err(|e| SbError::Filesystem {
            message: "error walking directory".to_string(),
            path: content_dir.display().to_string(),
            source: e.into_io_error(),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        // Unreadable files are skipped rather than aborting the whole scan.
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let description = match split_frontmatter(&content) {
            Some((fm, _)) if is_page_template(fm) => {
                frontmatter_scalar(fm, "description").filter(|s| !s.is_empty())
            }
            _ => continue,
        };
        let rel = path.strip_prefix(content_dir).unwrap_or(path);
        let name = path_to_page_name(rel).replace('\\', "/");
        out.push(TemplateInfo {
            name,
            source: TemplateSource::Local,
            description,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Interactive selection
// ---------------------------------------------------------------------------

/// Present templates and return the chosen name, or `None` if cancelled.
pub async fn pick_template(templates: &[TemplateInfo]) -> SbResult<Option<String>> {
    let names: Vec<String> = templates.iter().map(|t| t.name.clone()).collect();
    crate::commands::picker::pick(&names, "template").await
}

// ---------------------------------------------------------------------------
// Instantiation
// ---------------------------------------------------------------------------

/// Fetch a template's content and instantiate it into new-page content.
///
/// A SilverBullet page template carries its own frontmatter (`tags:
/// meta/template/page`, `command`, `suggestedName`, a nested `frontmatter:`
/// block for the new page, …) and a body that may contain `${...}` Space Lua
/// expressions and a `|^|` cursor marker. Instantiation injects the template's
/// `frontmatter:` block as the new page's frontmatter (dropping the template's
/// own metadata) and removes the `|^|` cursor marker from the body.
///
/// `cursor_fill` (piped stdin, if any) is spliced in at the `|^|` marker before
/// rendering ("splice then render"), so `${...}` in either the template or the
/// piped text resolves.
///
/// With the Runtime API available the result is then rendered so `${...}`
/// resolves; on any render failure we fall back to the unrendered instantiation.
pub async fn fetch_and_render(
    cli_token: Option<&str>,
    config: &ResolvedConfig,
    content_dir: &Path,
    template_name: &str,
    cursor_fill: Option<&str>,
) -> SbResult<String> {
    let raw = fetch_template_content(content_dir, template_name, config, cli_token)
        .await?
        .ok_or_else(|| SbError::PageNotFound {
            name: template_name.to_string(),
        })?;
    render_from_raw(cli_token, config, &raw, cursor_fill).await
}

/// Instantiate + render already-fetched template content.
async fn render_from_raw(
    cli_token: Option<&str>,
    config: &ResolvedConfig,
    raw: &str,
    cursor_fill: Option<&str>,
) -> SbResult<String> {
    // Unrendered new-page content: injected frontmatter + body, cursor spliced.
    // This is the offline result and the base for server rendering.
    let instantiated = instantiate_template(raw, cursor_fill);

    if config.runtime_available.value {
        match render_via_runtime(cli_token, &instantiated).await {
            Ok(rendered) => Ok(rendered),
            Err(e) => {
                tracing::warn!("template render failed, using unrendered content: {e}");
                Ok(instantiated)
            }
        }
    } else {
        Ok(instantiated)
    }
}

async fn render_via_runtime(cli_token: Option<&str>, body: &str) -> SbResult<String> {
    let client = build_client(cli_token)?;
    let script = format!(
        "local _tmpl = template.new({})\nreturn _tmpl({{}})",
        lua_long_string(body)
    );
    let result = eval_lua_result(&client, &script).await?;
    match result {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Null => Err(SbError::Config {
            message: "template render returned nil".into(),
        }),
        other => Ok(other.to_string()),
    }
}

/// Fetch template content: try local file first, then remote.
///
/// Returns `None` when the template cannot be found in either location. Shared
/// by `daily`, `page create`, and `template new`.
pub(crate) async fn fetch_template_content(
    content_dir: &Path,
    template_name: &str,
    config: &ResolvedConfig,
    cli_token: Option<&str>,
) -> SbResult<Option<String>> {
    let local_path = content_dir.join(format!("{}.md", template_name));
    if local_path.exists() {
        let content = std::fs::read_to_string(&local_path).map_err(|e| SbError::Filesystem {
            message: "failed to read template".into(),
            path: local_path.display().to_string(),
            source: Some(e),
        })?;
        return Ok(Some(content));
    }

    if let Some(ref url) = config.server_url.value {
        match crate::config::resolve_token(cli_token, config) {
            Ok(token) => {
                let client = SbClient::new(url, &token)?;
                match client.get_page(template_name).await {
                    Ok(content) => return Ok(Some(content)),
                    Err(SbError::PageNotFound { .. }) => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
            Err(SbError::TokenNotFound { .. }) => return Ok(None),
            Err(e) => return Err(e),
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Frontmatter helpers (dependency-free — matches the project's hand-rolled style)
// ---------------------------------------------------------------------------

/// Split a leading YAML frontmatter block from the body.
///
/// Returns `(frontmatter, body)` when the content starts with a `---` line and
/// has a closing `---` (or `...`) line; otherwise `None`. A leading UTF-8 BOM is
/// tolerated.
pub(crate) fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let c = content.strip_prefix('\u{feff}').unwrap_or(content);
    let after_open = c
        .strip_prefix("---\n")
        .or_else(|| c.strip_prefix("---\r\n"))?;
    let open_len = c.len() - after_open.len();
    let mut idx = 0usize;
    for line in after_open.split_inclusive('\n') {
        let t = line.trim_end_matches(['\r', '\n']);
        if t == "---" || t == "..." {
            let fm = &c[open_len..open_len + idx];
            let body = &c[open_len + idx + line.len()..];
            return Some((fm, body));
        }
        idx += line.len();
    }
    None
}

/// The cursor marker SilverBullet places in a template body to mark where the
/// caret should land after instantiation.
const CURSOR_MARKER: &str = "|^|";

/// Turn a raw template page into the content for a new page created from it.
///
/// The template's own frontmatter is dropped; its nested `frontmatter:` block
/// (if any) becomes the new page's frontmatter. The `|^|` cursor marker in the
/// body is replaced by `cursor_fill` (piped stdin) — or removed when
/// `cursor_fill` is `None`. When `cursor_fill` is `Some` but the body has no
/// marker, the fill is appended after the body. `${...}` expressions are left
/// intact for the caller to render (via the Runtime API) or leave literal when
/// offline. Content that isn't a recognizable page template is returned
/// unchanged apart from the cursor handling.
pub(crate) fn instantiate_template(content: &str, cursor_fill: Option<&str>) -> String {
    let (fm, body) = match split_frontmatter(content) {
        Some(parts) => parts,
        None => return splice_cursor(content, cursor_fill),
    };

    let body = splice_cursor(body, cursor_fill);
    let body = body.trim_start_matches('\n').to_string();

    match extract_frontmatter_payload(fm) {
        Some(front) => format!("---\n{}\n---\n\n{}", front.trim_end(), body),
        None => body,
    }
}

/// Replace the `|^|` cursor marker with `fill`. With no fill the marker is
/// simply removed. With a non-empty fill and no marker present, the fill is
/// appended after the text (separated by a newline).
fn splice_cursor(text: &str, fill: Option<&str>) -> String {
    let fill = fill.unwrap_or("");
    if text.contains(CURSOR_MARKER) {
        text.replace(CURSOR_MARKER, fill)
    } else if fill.is_empty() {
        text.to_string()
    } else {
        let sep = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{text}{sep}{fill}")
    }
}

/// Read piped stdin, returning `None` when stdin is a TTY (nothing piped). A
/// single trailing newline is trimmed so an inline `|^|` splice isn't forced
/// onto its own line.
pub(crate) fn read_piped_stdin() -> SbResult<Option<String>> {
    use std::io::Read;
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| SbError::Filesystem {
            message: "failed to read stdin".into(),
            path: "<stdin>".into(),
            source: Some(e),
        })?;
    let trimmed = buf
        .strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(&buf);
    Ok(Some(trimmed.to_string()))
}

/// Extract the nested `frontmatter:` YAML block-scalar from a template's
/// frontmatter, dedented to column zero. Returns `None` when the key is absent.
pub(crate) fn extract_frontmatter_payload(frontmatter: &str) -> Option<String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("frontmatter:") else {
            continue;
        };
        let key_indent = line.len() - trimmed.len();
        let rest = rest.trim();
        // Inline scalar (rare): `frontmatter: tags: x` — take it verbatim.
        if !rest.is_empty() && !matches!(rest, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            return Some(rest.to_string());
        }
        // Block scalar: gather following lines indented past the key.
        let mut payload: Vec<&str> = Vec::new();
        let mut common_indent: Option<usize> = None;
        for l in &lines[i + 1..] {
            if l.trim().is_empty() {
                payload.push("");
                continue;
            }
            let indent = l.len() - l.trim_start().len();
            if indent <= key_indent {
                break;
            }
            common_indent = Some(common_indent.map_or(indent, |c| c.min(indent)));
            payload.push(l);
        }
        let ci = common_indent.unwrap_or(0);
        let dedented = payload
            .iter()
            .map(|l| {
                if l.len() >= ci {
                    &l[ci..]
                } else {
                    l.trim_start()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let dedented = dedented.trim_end().to_string();
        return if dedented.is_empty() {
            None
        } else {
            Some(dedented)
        };
    }
    None
}

/// Read a top-level scalar frontmatter value (`key: value`), stripping
/// surrounding quotes. Only column-zero keys match, so nested block content
/// (e.g. inside the `frontmatter:` block) is never picked up. Returns `None`
/// when the key is absent.
pub(crate) fn frontmatter_scalar(frontmatter: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in frontmatter.lines() {
        if line.starts_with([' ', '\t']) {
            continue; // indented → nested, not a top-level key
        }
        if let Some(rest) = line.strip_prefix(&prefix) {
            let v = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            return Some(v.to_string());
        }
    }
    None
}

/// Read a top-level boolean frontmatter value. Returns `None` when absent or
/// unparseable.
pub(crate) fn frontmatter_bool(frontmatter: &str, key: &str) -> Option<bool> {
    match frontmatter_scalar(frontmatter, key)?
        .to_lowercase()
        .as_str()
    {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

/// Whether a YAML frontmatter block declares the page-template tag
/// (`meta/template/page`), across the common shapes: `tags: meta/template/page`,
/// `tags: [a, meta/template/page]`, and the block list (`tags:\n  - …`).
pub(crate) fn is_page_template(frontmatter: &str) -> bool {
    frontmatter_has_tag(frontmatter, PAGE_TEMPLATE_TAG)
}

fn frontmatter_has_tag(frontmatter: &str, target: &str) -> bool {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if let Some(rest) = trimmed.strip_prefix("tags:") {
            let val = rest.trim();
            if !val.is_empty() {
                if value_contains_tag(val, target) {
                    return true;
                }
            } else {
                // Block list: following `- item` lines (allowing blank lines).
                let mut j = i + 1;
                while j < lines.len() {
                    let item = lines[j].trim();
                    if let Some(v) = item.strip_prefix('-') {
                        if normalize_tag(v) == target {
                            return true;
                        }
                        j += 1;
                    } else if item.is_empty() {
                        j += 1;
                    } else {
                        break;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// Check an inline `tags:` value (scalar or flow list) for `target`.
fn value_contains_tag(val: &str, target: &str) -> bool {
    let val = val.trim();
    let inner = val
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(val);
    inner.split(',').any(|part| normalize_tag(part) == target)
}

/// Normalize a tag token: trim whitespace, surrounding quotes, and a leading `#`.
fn normalize_tag(s: &str) -> String {
    let s = s.trim();
    let s = s.trim_matches(|c| c == '"' || c == '\'');
    let s = s.strip_prefix('#').unwrap_or(s);
    s.trim().to_string()
}

/// Wrap `s` in a Lua long-bracket string, escalating the `=` level until the
/// delimiter can't collide with the content. A leading newline is added because
/// Lua ignores the first newline of a long string.
fn lua_long_string(s: &str) -> String {
    let mut eqs = String::new();
    loop {
        let open = format!("[{eqs}[");
        let close = format!("]{eqs}]");
        if !s.contains(&open) && !s.contains(&close) {
            return format!("[{eqs}[\n{s}]{eqs}]");
        }
        eqs.push('=');
    }
}

// ---------------------------------------------------------------------------
// Shared runtime-Lua evaluation
// ---------------------------------------------------------------------------

/// POST a Lua script to `/.runtime/lua_script` and return its `result` value.
/// Maps 503 to the standard runtime-unavailable error and surfaces Lua errors.
async fn eval_lua_result(client: &SbClient, script: &str) -> SbResult<serde_json::Value> {
    let resp = client.post_text("/.runtime/lua_script", script).await?;
    let status = resp.status();
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return Err(runtime_unavailable_error());
    }
    let url = format!("{}/.runtime/lua_script", client.base_url());
    let body = resp.text().await.map_err(|e| SbError::HttpStatus {
        status: status.as_u16(),
        url: url.clone(),
        body: format!("failed to read response: {e}"),
    })?;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url: url.clone(),
            body: format!("invalid JSON response: {e}"),
        })?;
    if let Some(error) = parsed.get("error").and_then(|e| e.as_str()) {
        return Err(SbError::HttpStatus {
            status: status.as_u16(),
            url,
            body: format!("Lua error: {error}"),
        });
    }
    Ok(parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- frontmatter parsing ---

    #[test]
    fn split_frontmatter_extracts_block_and_body() {
        let content = "---\ntags: template\n---\nBody here\n";
        let (fm, body) = split_frontmatter(content).expect("has frontmatter");
        assert_eq!(fm, "tags: template\n");
        assert_eq!(body, "Body here\n");
    }

    #[test]
    fn split_frontmatter_none_without_leading_delimiter() {
        assert!(split_frontmatter("no frontmatter here").is_none());
    }

    #[test]
    fn split_frontmatter_none_without_closing_delimiter() {
        assert!(split_frontmatter("---\ntags: template\nno close").is_none());
    }

    #[test]
    fn split_frontmatter_tolerates_bom() {
        let content = "\u{feff}---\ntags: template\n---\nx";
        assert!(split_frontmatter(content).is_some());
    }

    #[test]
    fn is_page_template_scalar() {
        assert!(is_page_template("tags: meta/template/page"));
        assert!(is_page_template("tags: \"meta/template/page\""));
        assert!(is_page_template("command: X\ntags: meta/template/page\n"));
    }

    #[test]
    fn is_page_template_flow_list() {
        assert!(is_page_template("tags: [meta, meta/template/page]"));
        assert!(is_page_template("tags: [meta/template/page]"));
        assert!(!is_page_template("tags: [meta, other]"));
    }

    #[test]
    fn is_page_template_block_list() {
        let fm = "title: X\ntags:\n  - meta\n  - meta/template/page\n";
        assert!(is_page_template(fm));
        let fm2 = "tags:\n  - meta\n  - other\n";
        assert!(!is_page_template(fm2));
    }

    #[test]
    fn is_page_template_false_when_absent() {
        assert!(!is_page_template("title: Hello\ntags: page"));
        assert!(!is_page_template(""));
        // A snippet template is not a page template.
        assert!(!is_page_template("tags: meta/template/snippet"));
    }

    #[test]
    fn is_page_template_does_not_match_substring() {
        assert!(!is_page_template("tags: [meta/template/pages, other]"));
        assert!(!is_page_template("tags: my-meta/template/page"));
    }

    // --- instantiation ---

    #[test]
    fn extract_frontmatter_payload_reads_block_scalar() {
        let fm = "tags: meta/template/page\ncommand: X\nfrontmatter: |\n  tags: project, hls\n  state: draft\nconfirmName: true\n";
        assert_eq!(
            extract_frontmatter_payload(fm).as_deref(),
            Some("tags: project, hls\nstate: draft")
        );
    }

    #[test]
    fn extract_frontmatter_payload_none_when_absent() {
        assert_eq!(
            extract_frontmatter_payload("tags: meta/template/page\n"),
            None
        );
    }

    #[test]
    fn instantiate_template_injects_frontmatter_and_strips_cursor() {
        let raw = "---\ntags: meta/template/page\nfrontmatter: |\n  tags: project\n  state: draft\n---\n\n# Intro\n\n|^|\n\n## Body\n";
        let out = instantiate_template(raw, None);
        assert_eq!(
            out,
            "---\ntags: project\nstate: draft\n---\n\n# Intro\n\n\n\n## Body\n"
        );
    }

    #[test]
    fn instantiate_template_body_only_when_no_frontmatter_key() {
        let raw = "---\ntags: meta/template/page\ncommand: Quick\n---\n# Note |^|\n";
        assert_eq!(instantiate_template(raw, None), "# Note \n");
    }

    #[test]
    fn instantiate_template_passthrough_without_frontmatter() {
        assert_eq!(instantiate_template("plain |^|body", None), "plain body");
    }

    #[test]
    fn instantiate_template_splices_cursor_fill_at_marker() {
        let raw = "---\ntags: meta/template/page\n---\n# Intro\n\n|^|\n\n## Body\n";
        let out = instantiate_template(raw, Some("piped text"));
        assert_eq!(out, "# Intro\n\npiped text\n\n## Body\n");
    }

    #[test]
    fn instantiate_template_appends_fill_when_no_marker() {
        // Body has no |^| but stdin was piped → append after the body.
        let raw = "---\ntags: meta/template/page\n---\n# Intro\n";
        assert_eq!(instantiate_template(raw, Some("piped")), "# Intro\npiped");
    }

    #[test]
    fn splice_cursor_removes_marker_without_fill() {
        assert_eq!(splice_cursor("a |^| b", None), "a  b");
        assert_eq!(splice_cursor("a |^| b", Some("X")), "a X b");
        assert_eq!(splice_cursor("no marker", Some("X")), "no marker\nX");
        assert_eq!(splice_cursor("no marker", None), "no marker");
    }

    // --- suggestedName / confirmName parsing ---

    #[test]
    fn frontmatter_scalar_reads_top_level_quoted_value() {
        let fm = "command: \"New: HLS\"\nsuggestedName: \"Projects/Work/\"\nconfirmName: true\n";
        assert_eq!(
            frontmatter_scalar(fm, "suggestedName").as_deref(),
            Some("Projects/Work/")
        );
        assert_eq!(
            frontmatter_scalar(fm, "command").as_deref(),
            Some("New: HLS")
        );
        assert_eq!(frontmatter_scalar(fm, "missing"), None);
    }

    #[test]
    fn frontmatter_scalar_ignores_nested_keys() {
        // A `suggestedName` nested inside the `frontmatter:` block must not match.
        let fm =
            "tags: meta/template/page\nfrontmatter: |\n  suggestedName: nope\n  state: draft\n";
        assert_eq!(frontmatter_scalar(fm, "suggestedName"), None);
        assert_eq!(frontmatter_scalar(fm, "state"), None);
    }

    #[test]
    fn frontmatter_bool_parses_true_false() {
        assert_eq!(
            frontmatter_bool("confirmName: true", "confirmName"),
            Some(true)
        );
        assert_eq!(
            frontmatter_bool("confirmName: false", "confirmName"),
            Some(false)
        );
        assert_eq!(frontmatter_bool("other: x", "confirmName"), None);
    }

    #[test]
    fn parse_name_hints_reads_all_fields_with_defaults() {
        let raw = "---\ntags: meta/template/page\nsuggestedName: \"Inbox/${x}\"\nconfirmName: false\nopenIfExists: true\n---\nbody";
        let h = parse_name_hints(raw);
        assert_eq!(h.suggested, "Inbox/${x}");
        assert!(!h.confirm);
        assert!(h.open_if_exists);

        // Defaults: confirmName true, openIfExists false when absent.
        let bare = parse_name_hints("---\ntags: meta/template/page\n---\nbody");
        assert_eq!(bare.suggested, "");
        assert!(bare.confirm);
        assert!(!bare.open_if_exists);
    }

    // --- lua long string ---

    #[test]
    fn lua_long_string_wraps_plain_content() {
        let out = lua_long_string("hello");
        assert_eq!(out, "[[\nhello]]");
    }

    #[test]
    fn lua_long_string_escalates_on_collision() {
        // Content containing `]]` forces a higher bracket level.
        let out = lua_long_string("a ]] b");
        assert!(out.starts_with("[=["), "got: {out}");
        assert!(out.ends_with("]=]"), "got: {out}");
    }

    // --- index row extraction ---

    #[test]
    fn row_name_reads_common_shapes() {
        assert_eq!(
            row_name(&serde_json::json!({"name": "A"})).as_deref(),
            Some("A")
        );
        assert_eq!(
            row_name(&serde_json::json!({"ref": "B"})).as_deref(),
            Some("B")
        );
        assert_eq!(row_name(&serde_json::json!("C")).as_deref(), Some("C"));
        assert_eq!(row_name(&serde_json::json!({"x": 1})), None);
    }

    // --- local scan discovery ---

    mod scan_tests {
        use super::*;
        use crate::test_util::{make_space, SbSpaceGuard};

        #[test]
        fn discover_via_scan_finds_tagged_pages() {
            let tmp = make_space(None);
            let root = tmp.path();
            std::fs::write(
                root.join("Meeting.md"),
                "---\ntags: meta/template/page\ndescription: Meeting notes\n---\n# ${title}\n",
            )
            .unwrap();
            std::fs::create_dir_all(root.join("Library")).unwrap();
            std::fs::write(
                root.join("Library/Daily.md"),
                "---\ntags:\n  - meta/template/page\n---\nbody",
            )
            .unwrap();
            std::fs::write(root.join("Regular.md"), "---\ntags: page\n---\nnope").unwrap();
            std::fs::write(root.join("Plain.md"), "just text").unwrap();

            let _g = SbSpaceGuard::set(root);
            let found = discover_via_scan(root).unwrap();
            let names: Vec<&str> = found.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(names, vec!["Library/Daily", "Meeting"]);
            assert!(found.iter().all(|t| t.source == TemplateSource::Local));
            // description parsed for Meeting, absent for Daily.
            let meeting = found.iter().find(|t| t.name == "Meeting").unwrap();
            assert_eq!(meeting.description.as_deref(), Some("Meeting notes"));
            let daily = found.iter().find(|t| t.name == "Library/Daily").unwrap();
            assert_eq!(daily.description, None);
        }
    }

    // --- command-level (wiremock) ---

    mod execute_tests {
        use super::*;
        use crate::test_util::{make_space, SbSpaceGuard};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn enable_runtime(space_root: &std::path::Path) {
            crate::config::update_config_value(
                &space_root.join(".sb"),
                "runtime",
                "available",
                true,
            )
            .unwrap();
        }

        #[tokio::test]
        async fn list_via_index_returns_names() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/.runtime/lua_script"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"result":[{"name":"Meeting"},{"name":"Daily"}]}"#),
                )
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            enable_runtime(tmp.path());
            let _g = SbSpaceGuard::set(tmp.path());

            let cfg = ResolvedConfig::load_from(tmp.path()).unwrap();
            let templates = discover_templates(None, &cfg).await.unwrap();
            let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
            // Sorted + deduped.
            assert_eq!(names, vec!["Daily", "Meeting"]);
        }

        #[tokio::test]
        async fn open_if_exists_does_not_error_when_page_exists() {
            // Runtime off; static suggestedName + confirmName:false so no prompt.
            let tmp = make_space(None);
            let root = tmp.path();
            std::fs::write(
                root.join("Tmpl.md"),
                "---\ntags: meta/template/page\nsuggestedName: Existing\nconfirmName: false\nopenIfExists: true\n---\nbody",
            )
            .unwrap();
            std::fs::write(root.join("Existing.md"), "already here").unwrap();
            let _g = SbSpaceGuard::set(root);

            execute_new(None, None, Some("Tmpl"), true, false, true, false)
                .await
                .expect("openIfExists should not error on existing page");
            // Existing page is left untouched (not overwritten by the template).
            assert_eq!(
                std::fs::read_to_string(root.join("Existing.md")).unwrap(),
                "already here"
            );
        }

        #[tokio::test]
        async fn errors_when_page_exists_without_open_if_exists() {
            let tmp = make_space(None);
            let root = tmp.path();
            std::fs::write(
                root.join("Tmpl.md"),
                "---\ntags: meta/template/page\nsuggestedName: Existing\nconfirmName: false\n---\nbody",
            )
            .unwrap();
            std::fs::write(root.join("Existing.md"), "already here").unwrap();
            let _g = SbSpaceGuard::set(root);

            let err = execute_new(None, None, Some("Tmpl"), true, false, true, false)
                .await
                .unwrap_err();
            assert!(matches!(err, SbError::PageAlreadyExists { .. }));
        }

        #[tokio::test]
        async fn list_execute_json_ok() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/.runtime/lua_script"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(r#"{"result":[{"name":"T"}]}"#),
                )
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            enable_runtime(tmp.path());
            let _g = SbSpaceGuard::set(tmp.path());

            execute_list(None, &OutputFormat::Json, true, false)
                .await
                .expect("list ok");
        }

        #[tokio::test]
        async fn fetch_and_render_offline_instantiates() {
            // No runtime → local file resolution + best-effort instantiation:
            // inject the template's `frontmatter:` block, drop `|^|`, ${..} literal.
            let tmp = make_space(None);
            std::fs::write(
                tmp.path().join("Tmpl.md"),
                "---\ntags: meta/template/page\nfrontmatter: |\n  tags: project\n---\n# Hello |^|\n",
            )
            .unwrap();
            let _g = SbSpaceGuard::set(tmp.path());
            let cfg = ResolvedConfig::load_from(tmp.path()).unwrap();
            let out = fetch_and_render(None, &cfg, tmp.path(), "Tmpl", None)
                .await
                .unwrap();
            assert_eq!(out, "---\ntags: project\n---\n\n# Hello \n");
        }

        #[tokio::test]
        async fn fetch_and_render_runtime_renders() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/.runtime/lua_script"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(r##"{"result":"# Rendered"}"##),
                )
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            enable_runtime(tmp.path());
            std::fs::write(
                tmp.path().join("Tmpl.md"),
                "---\ntags: meta/template/page\n---\n# ${x}\n",
            )
            .unwrap();
            let _g = SbSpaceGuard::set(tmp.path());
            let cfg = ResolvedConfig::load_from(tmp.path()).unwrap();
            let out = fetch_and_render(None, &cfg, tmp.path(), "Tmpl", None)
                .await
                .unwrap();
            assert_eq!(out, "# Rendered");
        }

        #[tokio::test]
        async fn fetch_and_render_runtime_error_falls_back_to_stripped() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/.runtime/lua_script"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(r#"{"error":"no such function"}"#),
                )
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            enable_runtime(tmp.path());
            std::fs::write(
                tmp.path().join("Tmpl.md"),
                "---\ntags: meta/template/page\n---\nBody only\n",
            )
            .unwrap();
            let _g = SbSpaceGuard::set(tmp.path());
            let cfg = ResolvedConfig::load_from(tmp.path()).unwrap();
            let out = fetch_and_render(None, &cfg, tmp.path(), "Tmpl", None)
                .await
                .unwrap();
            assert_eq!(out, "Body only\n");
        }
    }
}
