use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use jiff::civil::Date;

use crate::cli::OutputFormat;
use crate::commands::page::{find_space_root, open_in_editor, validate_page_path};
use crate::commands::server::build_client;
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};
use crate::output;

/// Resolve the target date given offset days from today.
///
/// offset=0 means today, offset=-1 means yesterday.
/// --yesterday is converted to offset=-1 by the caller.
pub fn resolve_daily_date(offset: i64) -> SbResult<Date> {
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
pub fn format_daily_path(path_template: &str, date_format: &str, date: &Date) -> String {
    let formatted_date = date.strftime(date_format).to_string();
    path_template.replace("{{date}}", &formatted_date)
}

/// Parse a YYYY-MM-DD date string (`--on`, `--from`, `--to`).
pub fn parse_iso_date(s: &str) -> SbResult<Date> {
    Date::strptime("%Y-%m-%d", s)
        .map_err(|e| SbError::Usage(format!("invalid date {:?}: expected YYYY-MM-DD ({})", s, e)))
}

/// Validate that a time string matches HH:MM.
pub fn validate_time_hhmm(s: &str) -> SbResult<()> {
    jiff::civil::Time::strptime("%H:%M", s)
        .map_err(|e| SbError::Usage(format!("invalid --time {:?}: expected HH:MM ({})", s, e)))?;
    Ok(())
}

/// Date prefix detected at the start of an entry's text.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum DatePrefix {
    Today,
    Yesterday,
    Iso(Date),
}

/// Detect a leading date prefix on a line of entry text.
///
/// Accepts `today: ...`, `yesterday: ...`, and `YYYY-MM-DD: ...` (case-insensitive
/// for the keywords). Returns the matched prefix and the rest of the entry text
/// with the prefix and one space stripped. Anything else returns `None`, including
/// malformed dates (so we don't misroute on a typo).
pub fn parse_date_prefix(text: &str) -> Option<(DatePrefix, String)> {
    let (head, rest) = text.split_once(':')?;
    let head_trim = head.trim();
    let rest_trim = rest.trim_start();
    match head_trim.to_lowercase().as_str() {
        "today" => Some((DatePrefix::Today, rest_trim.to_string())),
        "yesterday" => Some((DatePrefix::Yesterday, rest_trim.to_string())),
        _ => Date::strptime("%Y-%m-%d", head_trim)
            .ok()
            .map(|d| (DatePrefix::Iso(d), rest_trim.to_string())),
    }
}

/// Strip SilverBullet inline attributes `[key:: value]` from a string and collect
/// them into a map. Keys are restricted to ASCII alphanumerics / `_` / `-` to
/// avoid eating bracketed Markdown like `[link text](url)`.
pub fn parse_inline_attributes(input: &str) -> (BTreeMap<String, String>, String) {
    let mut attrs = BTreeMap::new();
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(rel) = input[i + 1..].find([']', '\n']) {
                if bytes[i + 1 + rel] == b']' {
                    let inner = &input[i + 1..i + 1 + rel];
                    if let Some(sep) = inner.find("::") {
                        let key = inner[..sep].trim();
                        let value = inner[sep + 2..].trim();
                        let key_ok = !key.is_empty()
                            && key
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                        if key_ok {
                            attrs.insert(key.to_string(), value.to_string());
                            i = i + 1 + rel + 1;
                            if i < bytes.len() && bytes[i] == b' ' {
                                i += 1;
                            }
                            continue;
                        }
                    }
                }
            }
        }
        let ch_len = input[i..].chars().next().unwrap().len_utf8();
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    let trimmed = out
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    (attrs, trimmed)
}

/// Extract `#tag` tokens from a string. Tags must be at start of string or
/// preceded by whitespace; tag chars are alphanumerics / `_` / `-` / `/`.
pub fn extract_tags(s: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let preceded_by_space = i == 0 || bytes[i - 1].is_ascii_whitespace();
            if preceded_by_space {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() {
                    let c = bytes[end] as char;
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                if end > start {
                    tags.push(s[start..end].to_string());
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    tags
}

/// Format a journal entry as a SilverBullet bullet line.
///
/// When `task` is true the line becomes a checkbox item (`* [ ] ...`) so
/// SilverBullet indexes it as a task; `tag` (without a leading `#`), when set,
/// is appended to the first line as a `#tag` so tag-gated task indexing picks
/// it up. Multi-line bodies use 2-space continuation indentation so the whole
/// entry remains a single bullet item.
pub fn format_entry(
    time: Option<&str>,
    star: bool,
    bullet: char,
    body: &str,
    task: bool,
    tag: Option<&str>,
) -> String {
    let body = body.trim();
    let mut prefix = String::new();
    if task {
        prefix.push_str("[ ] ");
    }
    if let Some(t) = time {
        prefix.push_str(&format!("[time:: {}] ", t));
    }
    if star {
        prefix.push_str("[starred:: true] ");
    }
    let mut lines = body.lines();
    let first = lines.next().unwrap_or("");
    let mut out = format!("{} {}{}", bullet, prefix, first);
    if let Some(tag) = tag {
        let tag = tag.trim().trim_start_matches('#');
        if !tag.is_empty() {
            out.push_str(&format!(" #{}", tag));
        }
    }
    for line in lines {
        out.push('\n');
        if !line.trim().is_empty() {
            out.push_str("  ");
            out.push_str(line);
        }
    }
    out
}

/// One parsed entry from a daily note file.
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub date: Date,
    pub time: Option<String>,
    pub starred: bool,
    pub tags: Vec<String>,
    pub body: String,
    pub attrs: BTreeMap<String, String>,
    pub source_page: String,
}

/// Parse all top-level bullets in `content` into `JournalEntry` values.
///
/// A bullet starts with `* ` or `- ` at the beginning of a line. Subsequent
/// lines that begin with at least 2 spaces are treated as continuation of the
/// same bullet, as are blank lines followed by more indented lines. Anything
/// else terminates the bullet.
pub fn parse_entries(content: &str, date: Date, source_page: &str) -> Vec<JournalEntry> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let after = if let Some(rest) = line.strip_prefix("* ") {
            Some(rest)
        } else {
            line.strip_prefix("- ")
        };
        if let Some(first) = after {
            let mut body_lines: Vec<String> = vec![first.to_string()];
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j];
                if next.trim().is_empty() {
                    // A blank line stays inside the bullet when the next
                    // non-blank line is still indented: that is a paragraph
                    // break within one entry, as in Markdown. Editors strip
                    // the `  ` that `sb daily` used to write on these lines.
                    let resumes = lines[j..]
                        .iter()
                        .find(|l| !l.trim().is_empty())
                        .is_some_and(|l| l.starts_with("  "));
                    if !resumes {
                        break;
                    }
                    body_lines.push(String::new());
                    j += 1;
                } else if let Some(rest) = next.strip_prefix("  ") {
                    body_lines.push(rest.to_string());
                    j += 1;
                } else {
                    break;
                }
            }
            let raw = body_lines.join("\n");
            let (attrs, body) = parse_inline_attributes(&raw);
            let time = attrs.get("time").cloned();
            let starred = attrs
                .get("starred")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let tags = extract_tags(&body);
            out.push(JournalEntry {
                date,
                time,
                starred,
                tags,
                body,
                attrs,
                source_page: source_page.to_string(),
            });
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Return the directory portion of a daily path template.
///
/// Only templates where `{{date}}` is the final path segment are supported.
/// `"Journal/{{date}}"` → `"Journal"`. `"{{date}}"` → `""`.
pub fn daily_dir_template(path_template: &str) -> SbResult<String> {
    let idx = path_template
        .find("{{date}}")
        .ok_or_else(|| SbError::Config {
            message: "daily.path must contain {{date}}".into(),
        })?;
    let after = &path_template[idx + "{{date}}".len()..];
    if !after.is_empty() {
        return Err(SbError::Config {
            message: format!(
                "daily.path read mode requires {{{{date}}}} to be the final path segment; suffix {:?} is not supported",
                after
            ),
        });
    }
    Ok(path_template[..idx].trim_end_matches('/').to_string())
}

/// Discover daily-note files in the space and pair each with its parsed date.
pub fn list_daily_files(
    space_root: &Path,
    daily_dir: &str,
    date_format: &str,
) -> SbResult<Vec<(PathBuf, Date)>> {
    let dir = if daily_dir.is_empty() {
        space_root.to_path_buf()
    } else {
        space_root.join(daily_dir)
    };
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let read_dir = std::fs::read_dir(&dir).map_err(|e| SbError::Filesystem {
        message: "failed to read daily directory".into(),
        path: dir.display().to_string(),
        source: Some(e),
    })?;
    for entry in read_dir {
        let entry = entry.map_err(|e| SbError::Filesystem {
            message: "failed to enumerate daily entry".into(),
            path: dir.display().to_string(),
            source: Some(e),
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if let Ok(date) = Date::strptime(date_format, stem) {
            out.push((path, date));
        }
    }
    Ok(out)
}

// Template fetching (local file first, then remote) is shared with
// `page create` and `template new`; the implementation lives in the template
// module. Re-exported here so existing call sites and tests keep resolving
// `fetch_template_content` within this module.
use crate::commands::template::fetch_template_content;

/// Ensure the day's note file exists. Creates parent dirs and applies the
/// configured template if the file is being created for the first time.
/// Returns `true` when the file was just created.
pub(crate) async fn ensure_day_file(
    page_path: &Path,
    content_dir: &Path,
    config: &ResolvedConfig,
    cli_token: Option<&str>,
    date: &Date,
) -> SbResult<bool> {
    if page_path.exists() {
        return Ok(false);
    }
    if let Some(parent) = page_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
            message: "failed to create directory".into(),
            path: parent.display().to_string(),
            source: Some(e),
        })?;
    }
    let content = match config.daily_template.value.as_deref() {
        Some(name) => match fetch_template_content(content_dir, name, config, cli_token).await? {
            Some(raw) => render_daily_template(&raw, date, config, cli_token).await,
            None => String::new(),
        },
        None => String::new(),
    };
    std::fs::write(page_path, &content).map_err(|e| SbError::Filesystem {
        message: "failed to create daily note".into(),
        path: page_path.display().to_string(),
        source: Some(e),
    })?;
    Ok(true)
}

/// Fill `{{date}}` / `{{weekday}}` placeholders with the note's own date.
///
/// The note's date, not today's: a note created by `--on` or `--yesterday`
/// must title itself correctly, which SilverBullet's `${date.today()}` cannot.
/// `{{date}}` uses `daily.dateFormat`, the same token and format as `daily.path`.
pub fn fill_date_placeholders(raw: &str, date: &Date, date_format: &str) -> String {
    raw.replace("{{date}}", &date.strftime(date_format).to_string())
        .replace("{{weekday}}", &date.strftime("%A").to_string())
}

/// Turn a daily template's raw content into the new note's content.
///
/// Date placeholders are filled first. A real SilverBullet page template
/// (tagged `meta/template/page`) is then instantiated the way SilverBullet does
/// it: the template's own frontmatter is replaced by its `frontmatter:` payload,
/// so the template tag never lands on the note, and its `${...}` is rendered
/// through the Runtime API when available (a render failure keeps the
/// unrendered text). A plain file is used as is, `${...}` included, exactly as
/// SilverBullet leaves a plain page: a shell snippet's `${VAR}` survives.
pub(crate) async fn render_daily_template(
    raw: &str,
    date: &Date,
    config: &ResolvedConfig,
    cli_token: Option<&str>,
) -> String {
    use crate::commands::template::{
        instantiate_template, is_page_template, render_via_runtime, split_frontmatter,
    };
    let filled = fill_date_placeholders(raw, date, &config.daily_date_format.value);
    let is_template = split_frontmatter(&filled).is_some_and(|(fm, _)| is_page_template(fm));
    if !is_template {
        return filled;
    }
    let content = instantiate_template(&filled, None);
    if !content.contains("${") || !config.runtime_available.value {
        return content;
    }
    match render_via_runtime(cli_token, &content).await {
        Ok(rendered) => rendered,
        Err(e) => {
            tracing::warn!("daily template render failed, using unrendered content: {e}");
            content
        }
    }
}

/// Append a formatted entry to the day's note, prefixing with a newline when
/// the file already has trailing content.
pub(crate) fn append_entry(page_path: &Path, entry_md: &str, spacing: bool) -> SbResult<()> {
    let existing = std::fs::read(page_path).unwrap_or_default();
    let prefix = if existing.is_empty() {
        ""
    } else if !spacing {
        if existing.ends_with(b"\n") {
            ""
        } else {
            "\n"
        }
    } else {
        // Newlines inside the trailing whitespace run: 0 = mid-line, 1 = line
        // ended, 2+ = a blank line is already there (also for CRLF endings
        // and whitespace-only lines such as an indented continuation blank).
        let newlines = existing
            .iter()
            .rev()
            .take_while(|b| b.is_ascii_whitespace())
            .filter(|&&b| b == b'\n')
            .count();
        match newlines {
            0 => "\n\n",
            1 => "\n",
            _ => "",
        }
    };

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(page_path)
        .map_err(|e| SbError::Filesystem {
            message: "failed to open daily note for append".into(),
            path: page_path.display().to_string(),
            source: Some(e),
        })?;

    file.write_all(format!("{}{}\n", prefix, entry_md).as_bytes())
        .map_err(|e| SbError::Filesystem {
            message: "failed to append to daily note".into(),
            path: page_path.display().to_string(),
            source: Some(e),
        })
}

/// Args bundle for `execute`. Most fields are owned to keep the signature
/// matching the main dispatcher cleanly.
pub struct DailyArgs<'a> {
    pub cli_token: Option<&'a str>,
    pub entry: Vec<String>,
    pub yesterday: bool,
    pub offset: Option<i64>,
    pub on: Option<&'a str>,
    pub star: bool,
    pub time: Option<&'a str>,
    pub no_time: bool,
    pub task: bool,
    pub task_tag: Option<&'a str>,
    pub no_task_tag: bool,
    pub append: Option<&'a str>,
    pub sign: &'a [String],
    pub limit: Option<usize>,
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub contains: Option<&'a str>,
    pub tags: Vec<String>,
    pub starred: bool,
    pub short: bool,
    pub format: &'a OutputFormat,
    pub quiet: bool,
    pub color: bool,
}

/// Execute the `sb daily` command.
pub async fn execute(args: DailyArgs<'_>) -> SbResult<()> {
    let space_root = find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;
    let content_dir = space_root.join(&config.sync_dir.value);

    let stdin_piped = !std::io::stdin().is_terminal();
    let has_write_input = !args.entry.is_empty() || stdin_piped || args.append.is_some();

    let is_read = args.limit.is_some()
        || args.from.is_some()
        || args.to.is_some()
        || args.contains.is_some()
        || !args.tags.is_empty()
        || args.starred
        || args.short
        || (args.on.is_some() && !has_write_input);

    if is_read {
        return execute_read(&content_dir, &config, &args).await;
    }

    execute_write_or_editor(&content_dir, &config, &args, stdin_piped).await
}

/// Write/editor dispatcher: pulls entry text from positional args, stdin, or
/// `--append`; honours date-prefix routing; opens the editor when no text is
/// provided.
async fn execute_write_or_editor(
    space_root: &Path,
    config: &ResolvedConfig,
    args: &DailyArgs<'_>,
    stdin_piped: bool,
) -> SbResult<()> {
    let mut entry_text = args.entry.join(" ").trim().to_string();

    if entry_text.is_empty() && stdin_piped {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| SbError::Filesystem {
                message: "failed to read stdin".into(),
                path: "<stdin>".into(),
                source: Some(e),
            })?;
        entry_text = buf.trim_end_matches('\n').to_string();
    }

    if entry_text.is_empty() {
        if let Some(a) = args.append {
            entry_text = a.to_string();
        }
    }

    let mut prefix_date: Option<Date> = None;
    if !entry_text.is_empty() {
        if let Some((prefix, rest)) = parse_date_prefix(&entry_text) {
            prefix_date = Some(match prefix {
                DatePrefix::Today => resolve_daily_date(0)?,
                DatePrefix::Yesterday => resolve_daily_date(-1)?,
                DatePrefix::Iso(d) => d,
            });
            entry_text = rest;
        }
    }

    let date = if let Some(d) = prefix_date {
        if args.yesterday || args.offset.is_some() || args.on.is_some() {
            return Err(SbError::Usage(
                "date prefix in entry conflicts with --yesterday/--offset/--on".into(),
            ));
        }
        d
    } else if let Some(s) = args.on {
        parse_iso_date(s)?
    } else {
        let off = if args.yesterday {
            -1
        } else {
            args.offset.unwrap_or(0)
        };
        resolve_daily_date(off)?
    };

    let page_name = format_daily_path(
        &config.daily_path.value,
        &config.daily_date_format.value,
        &date,
    );
    let page_path = validate_page_path(space_root, &page_name)?;

    // --task (and the tag override flags, which imply it) require a body: a task
    // with no text is meaningless, so we never fall through to the editor.
    let is_task = args.task || args.task_tag.is_some() || args.no_task_tag;

    if entry_text.is_empty() {
        if is_task {
            return Err(SbError::Usage(
                "--task requires entry text (a task needs a body)".into(),
            ));
        }
        let created =
            ensure_day_file(&page_path, space_root, config, args.cli_token, &date).await?;
        if created {
            output::print_success(&format!("Created {}", page_name), args.color, args.quiet);
        }
        // Opening the note in $EDITOR is the interactive default; when running
        // non-interactively the note is already ensured to exist, so we stop
        // here instead of blocking a script on an editor it cannot drive.
        if crate::output::no_input() {
            return Ok(());
        }
        return open_in_editor(&page_path).await;
    }

    let created = ensure_day_file(&page_path, space_root, config, args.cli_token, &date).await?;
    if created {
        output::print_success(&format!("Created {}", page_name), args.color, args.quiet);
    }

    let time_str: Option<String> = if args.no_time {
        None
    } else if let Some(t) = args.time {
        validate_time_hhmm(t)?;
        Some(t.to_string())
    } else {
        Some(
            jiff::Zoned::now()
                .strftime(&config.daily_time_format.value)
                .to_string(),
        )
    };

    let bullet_char = config
        .daily_bullet_style
        .value
        .chars()
        .next()
        .unwrap_or('*');
    let task_tag = if is_task {
        resolve_task_tag(config, args).await
    } else {
        None
    };
    let entry_md = format_entry(
        time_str.as_deref(),
        args.star,
        bullet_char,
        &entry_text,
        is_task,
        task_tag.as_deref(),
    );
    // A daily entry is always a list item, so this lands the signature
    // inline at the end of the bullet rather than on a new (unindented)
    // line below it. See `page::append_signature` for the full rationale.
    let entry_md = crate::commands::page::append_signature(&entry_md, args.sign);

    append_entry(&page_path, &entry_md, config.daily_entry_spacing.value)?;
    output::print_success(
        &format!("Appended to {}", page_name),
        args.color,
        args.quiet,
    );
    Ok(())
}

/// Resolve the tag to append to a task line.
///
/// Precedence: `--no-task-tag` (none) > `--task-tag <TAG>` > `daily.taskTagMode`.
/// In `auto` mode the space's `index.task.all` setting is read: SilverBullet
/// indexes only tagged tasks when it is `false`, so we tag then and leave the
/// task bare when it is `true`. If the setting cannot be read we tag anyway,
/// since a tagged task is indexed under either setting.
async fn resolve_task_tag(config: &ResolvedConfig, args: &DailyArgs<'_>) -> Option<String> {
    if args.no_task_tag {
        return None;
    }
    if let Some(tag) = args.task_tag {
        return Some(tag.to_string());
    }

    let default_tag = config.daily_task_tag.value.clone();
    match config.daily_task_tag_mode.value.as_str() {
        "never" => None,
        "always" => Some(default_tag),
        _ => match fetch_index_task_all(config, args.cli_token).await {
            Some(true) => None,
            Some(false) => Some(default_tag),
            None => {
                if !args.quiet {
                    eprintln!(
                        "note: could not read index.task.all; tagging task with #{} to ensure it is indexed",
                        default_tag
                    );
                }
                Some(default_tag)
            }
        },
    }
}

/// Read the space's `index.task.all` setting via the runtime Lua endpoint.
///
/// Returns `None` when the runtime is unavailable or the request/parse fails,
/// so callers can fall back rather than failing the write.
async fn fetch_index_task_all(config: &ResolvedConfig, cli_token: Option<&str>) -> Option<bool> {
    if !config.runtime_available.value {
        return None;
    }
    let client = build_client(cli_token).ok()?;
    let resp = client
        .post_text("/.runtime/lua", "config.get(\"index.task.all\", true)")
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
    parsed.get("result")?.as_bool()
}

/// Filters resolved from CLI flags, applied during read mode.
struct ReadFilters {
    from: Option<Date>,
    to: Option<Date>,
    on: Option<Date>,
    contains: Option<String>,
    tags: Vec<String>,
    starred: bool,
}

impl ReadFilters {
    fn matches(&self, e: &JournalEntry) -> bool {
        if let Some(d) = self.on {
            if e.date != d {
                return false;
            }
        }
        if let Some(d) = self.from {
            if e.date < d {
                return false;
            }
        }
        if let Some(d) = self.to {
            if e.date > d {
                return false;
            }
        }
        if let Some(ref s) = self.contains {
            let needle = s.to_lowercase();
            if !e.body.to_lowercase().contains(&needle) {
                return false;
            }
        }
        if !self.tags.is_empty() {
            let entry_tags: Vec<String> = e.tags.iter().map(|t| t.to_lowercase()).collect();
            let any = self
                .tags
                .iter()
                .any(|t| entry_tags.contains(&t.to_lowercase()));
            if !any {
                return false;
            }
        }
        e.starred || !self.starred
    }
}

/// Pure read pipeline: discover daily files, parse, filter, sort, truncate.
/// Extracted from `execute_read` so it can be unit-tested without touching stdout.
pub fn collect_entries(
    space_root: &Path,
    config: &ResolvedConfig,
    args: &DailyArgs<'_>,
) -> SbResult<Vec<JournalEntry>> {
    let dir = daily_dir_template(&config.daily_path.value)?;
    let files = list_daily_files(space_root, &dir, &config.daily_date_format.value)?;

    let on = if let Some(s) = args.on {
        Some(parse_iso_date(s)?)
    } else if args.yesterday {
        Some(resolve_daily_date(-1)?)
    } else if let Some(off) = args.offset {
        Some(resolve_daily_date(off)?)
    } else {
        None
    };

    let contains_text = match args.contains {
        Some(s) => Some(s.to_string()),
        None if !args.entry.is_empty() => Some(args.entry.join(" ")),
        _ => None,
    };

    let filters = ReadFilters {
        from: args.from.map(parse_iso_date).transpose()?,
        to: args.to.map(parse_iso_date).transpose()?,
        on,
        contains: contains_text,
        tags: args.tags.clone(),
        starred: args.starred,
    };

    let mut entries: Vec<JournalEntry> = Vec::new();
    for (path, date) in files {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let page_name = if dir.is_empty() {
            stem
        } else {
            format!("{}/{}", dir, stem)
        };
        for entry in parse_entries(&content, date, &page_name) {
            if filters.matches(&entry) {
                entries.push(entry);
            }
        }
    }

    // Newest first. The sort is stable, so entries sharing a timestamp keep
    // their order in the file: an entry stays ahead of the tasks it filed.
    entries.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.time.cmp(&a.time)));

    if let Some(n) = args.limit {
        entries.truncate(n);
    }

    Ok(entries)
}

async fn execute_read(
    space_root: &Path,
    config: &ResolvedConfig,
    args: &DailyArgs<'_>,
) -> SbResult<()> {
    let entries = collect_entries(space_root, config, args)?;
    if entries.is_empty() && matches!(args.format, OutputFormat::Human) && !args.quiet {
        eprintln!(
            "No journal entries found (daily notes are read from `{}`).",
            config.daily_path.value
        );
    }
    // Wrap to the terminal only when a person is reading it; a pipe or file
    // gets one line per source line so grep and diff stay meaningful.
    let width = if output::is_tty() {
        console::Term::stdout()
            .size_checked()
            .map(|(_, w)| (w as usize).min(MAX_WRAP_WIDTH))
    } else {
        None
    };
    let style = RenderStyle {
        color: args.color,
        width,
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_entries_styled(&entries, args.format, args.short, style, &mut handle)
}

/// Lines wrap at the terminal width, but never wider than this: long measure
/// is hard to read on a wide terminal.
const MAX_WRAP_WIDTH: usize = 110;

/// Indent of an entry's body under its `  HH:MM  ` gutter.
const BODY_INDENT: usize = 9;

/// How human output is decorated.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderStyle {
    /// ANSI styling (date headers, times, links, tags, attributes).
    pub color: bool,
    /// Wrap body text to this many columns; `None` keeps source lines intact.
    pub width: Option<usize>,
}

/// Render entries with no color and no wrapping (stable output for tests and pipes).
pub fn render_entries(
    entries: &[JournalEntry],
    format: &OutputFormat,
    short: bool,
    out: &mut dyn Write,
) -> SbResult<()> {
    render_entries_styled(entries, format, short, RenderStyle::default(), out)
}

pub fn render_entries_styled(
    entries: &[JournalEntry],
    format: &OutputFormat,
    short: bool,
    style: RenderStyle,
    out: &mut dyn Write,
) -> SbResult<()> {
    let write_err = |e: std::io::Error| SbError::Filesystem {
        message: "failed to write entries".into(),
        path: "<stdout>".into(),
        source: Some(e),
    };
    match format {
        OutputFormat::Json => {
            #[derive(serde::Serialize)]
            struct JsonEntry<'a> {
                date: String,
                time: Option<&'a str>,
                starred: bool,
                tags: &'a [String],
                body: &'a str,
                attrs: &'a BTreeMap<String, String>,
                source_page: &'a str,
            }
            let json: Vec<JsonEntry> = entries
                .iter()
                .map(|e| JsonEntry {
                    date: e.date.strftime("%Y-%m-%d").to_string(),
                    time: e.time.as_deref(),
                    starred: e.starred,
                    tags: &e.tags,
                    body: &e.body,
                    attrs: &e.attrs,
                    source_page: &e.source_page,
                })
                .collect();
            let s = serde_json::to_string_pretty(&json).map_err(|e| SbError::Config {
                message: format!("failed to serialize entries: {e}"),
            })?;
            writeln!(out, "{}", s).map_err(write_err)?;
        }
        OutputFormat::Human if short => {
            for e in entries {
                let first = e.body.lines().next().unwrap_or("");
                let truncated: String = if first.chars().count() > 80 {
                    let mut s: String = first.chars().take(79).collect();
                    s.push('…');
                    s
                } else {
                    first.to_string()
                };
                writeln!(
                    out,
                    "{} {}  {}{}",
                    paint(
                        style.color,
                        Paint::Dim,
                        &e.date.strftime("%Y-%m-%d").to_string()
                    ),
                    time_cell(e, style.color),
                    star_cell(e, style.color),
                    highlight_inline(&truncated, style.color, None)
                )
                .map_err(write_err)?;
            }
        }
        OutputFormat::Human => {
            let mut last_date: Option<Date> = None;
            for e in entries {
                if last_date != Some(e.date) {
                    if last_date.is_some() {
                        writeln!(out).map_err(write_err)?;
                    }
                    let header = e.date.strftime("%A %Y-%m-%d").to_string();
                    writeln!(out, "{}", paint(style.color, Paint::Header, &header))
                        .map_err(write_err)?;
                    last_date = Some(e.date);
                }
                for line in render_entry_lines(e, style) {
                    writeln!(out, "{}", line).map_err(write_err)?;
                }
                writeln!(out).map_err(write_err)?;
            }
        }
    }
    Ok(())
}

/// The lines of one entry: `  HH:MM  ★ headline`, a dim attribute line, the
/// wrapped body, and a dim signature line, all hung under a 9-column gutter.
fn render_entry_lines(e: &JournalEntry, style: RenderStyle) -> Vec<String> {
    let (text, signature) = split_signature(&e.body);
    let mut lines = text.lines();
    let headline = lines.next().unwrap_or("");
    let pad = " ".repeat(BODY_INDENT);
    let wrap_at = style.width.map(|w| w.saturating_sub(BODY_INDENT).max(20));
    let mut out = Vec::new();

    let mut in_code = headline.trim_start().starts_with("```");
    let star_width = if e.starred { 2 } else { 0 };
    let head_width = wrap_at.map(|w| w.saturating_sub(star_width).max(20));
    let mut head = wrap_line(headline, head_width).into_iter();
    let first = head.next().unwrap_or_default();
    let first = if in_code {
        paint(style.color, Paint::Dim, &first)
    } else {
        highlight_inline(
            &checkbox(&first, style.color),
            style.color,
            Some(Paint::Bold),
        )
    };
    out.push(format!(
        "  {}  {}{}",
        time_cell(e, style.color),
        star_cell(e, style.color),
        first
    ));
    for l in head {
        out.push(format!(
            "{pad}{}",
            highlight_inline(&l, style.color, Some(Paint::Bold))
        ));
    }

    let extra: Vec<String> = e
        .attrs
        .iter()
        .filter(|(k, _)| k.as_str() != "time" && k.as_str() != "starred")
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    if !extra.is_empty() {
        out.push(format!(
            "{pad}{}",
            paint(style.color, Paint::Dim, &extra.join(" · "))
        ));
    }

    for line in lines {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push(format!("{pad}{}", paint(style.color, Paint::Dim, line)));
            continue;
        }
        if in_code {
            out.push(format!("{pad}{}", paint(style.color, Paint::Code, line)));
            continue;
        }
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        for l in wrap_line(line, wrap_at) {
            out.push(format!(
                "{pad}{}",
                highlight_inline(&checkbox(&l, style.color), style.color, None)
            ));
        }
    }
    if let Some(sig) = signature {
        out.push(format!(
            "{pad}{}",
            paint(style.color, Paint::Dim, &format!("-- {sig}"))
        ));
    }
    out
}

/// Split a trailing `-- @name [@name…]` signature (what `--sign` writes) off
/// the body. Returns the remaining text and the signers.
fn split_signature(body: &str) -> (String, Option<String>) {
    let trimmed = body.trim_end();
    let (head, last) = match trimmed.rfind('\n') {
        Some(i) => (&trimmed[..i + 1], &trimmed[i + 1..]),
        None => ("", trimmed),
    };
    let (before, sig) = match last.rfind("-- @") {
        Some(i) if i == 0 || last[..i].ends_with(' ') => (&last[..i], &last[i + 3..]),
        _ => return (body.to_string(), None),
    };
    if !sig
        .split_whitespace()
        .all(|w| w.starts_with('@') && w.len() > 1)
    {
        return (body.to_string(), None);
    }
    let text = format!("{head}{}", before.trim_end());
    (text.trim_end().to_string(), Some(sig.trim().to_string()))
}

/// Greedy word wrap that keeps a line's leading indent, and hangs list items
/// (`- `, `* `, `1. `) so their continuation lines align with the item text.
fn wrap_line(line: &str, width: Option<usize>) -> Vec<String> {
    let Some(width) = width else {
        return vec![line.to_string()];
    };
    if console::measure_text_width(line) <= width {
        return vec![line.to_string()];
    }
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    let marker_len = list_marker_len(rest);
    let hang = format!("{indent}{}", " ".repeat(marker_len));

    let mut out = Vec::new();
    let mut cur = indent.to_string();
    let mut cur_has_word = false;
    for word in rest.split(' ').filter(|w| !w.is_empty()) {
        let needed = if cur_has_word { 1 } else { 0 } + console::measure_text_width(word);
        if cur_has_word && console::measure_text_width(&cur) + needed > width {
            out.push(std::mem::replace(&mut cur, hang.clone()));
            cur_has_word = false;
        }
        if cur_has_word {
            cur.push(' ');
        }
        cur.push_str(word);
        cur_has_word = true;
    }
    if cur_has_word {
        out.push(cur);
    }
    out
}

fn list_marker_len(s: &str) -> usize {
    if s.starts_with("- ") || s.starts_with("* ") || s.starts_with("+ ") {
        return 2;
    }
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && s[digits..].starts_with(". ") {
        return digits + 2;
    }
    0
}

#[derive(Clone, Copy)]
enum Paint {
    Header,
    Bold,
    Dim,
    Time,
    Star,
    Link,
    Tag,
    Code,
    Open,
    Done,
}

fn paint(color: bool, p: Paint, s: &str) -> String {
    if !color || s.is_empty() {
        return s.to_string();
    }
    let st = console::Style::new().force_styling(true);
    let st = match p {
        Paint::Header => st.bold().underlined(),
        Paint::Bold => st.bold(),
        Paint::Dim => st.dim(),
        Paint::Time => st.cyan(),
        Paint::Star => st.yellow(),
        Paint::Link => st.blue(),
        Paint::Tag => st.magenta(),
        Paint::Code => st.yellow(),
        Paint::Open => st.yellow().bold(),
        Paint::Done => st.green(),
    };
    st.apply_to(s).to_string()
}

fn time_cell(e: &JournalEntry, color: bool) -> String {
    match e.time.as_deref() {
        Some(t) => paint(color, Paint::Time, t),
        None => paint(color, Paint::Dim, "--:--"),
    }
}

fn star_cell(e: &JournalEntry, color: bool) -> String {
    if e.starred {
        format!("{} ", paint(color, Paint::Star, "★"))
    } else {
        String::new()
    }
}

/// Color a leading task checkbox: open `[ ]` stands out, done `[x]` recedes.
fn checkbox(line: &str, color: bool) -> String {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let rest_after_marker = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .map(|r| (&rest[..2], r))
        .unwrap_or(("", rest));
    let (marker, body) = rest_after_marker;
    for (box_, p) in [
        ("[ ] ", Paint::Open),
        ("[x] ", Paint::Done),
        ("[X] ", Paint::Done),
    ] {
        if let Some(after) = body.strip_prefix(box_) {
            return format!(
                "{indent}{marker}{}{after}",
                paint(color, p, box_.trim_end()) + " "
            );
        }
    }
    line.to_string()
}

/// Color `[[wikilinks]]`, `#tags` and `` `code` `` within one line. Unclosed
/// spans (a link split by wrapping) are left plain.
fn highlight_inline(line: &str, color: bool, base: Option<Paint>) -> String {
    if !color {
        return line.to_string();
    }
    // Plain stretches are painted with `base` one by one, so a styled span's
    // reset code never cancels the base style (bold headline) after it.
    let mut out = String::with_capacity(line.len() + 16);
    let mut plain = String::new();
    let mut at_word_start = true;
    let flush = |out: &mut String, plain: &mut String| {
        if !plain.is_empty() {
            match base {
                Some(b) => out.push_str(&paint(true, b, plain)),
                None => out.push_str(plain),
            }
            plain.clear();
        }
    };
    let mut rest = line;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("[[") {
            if let Some(end) = after.find("]]") {
                flush(&mut out, &mut plain);
                out.push_str(&paint(true, Paint::Link, &rest[..end + 4]));
                rest = &after[end + 2..];
                at_word_start = false;
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                flush(&mut out, &mut plain);
                out.push_str(&paint(true, Paint::Code, &rest[..end + 2]));
                rest = &after[end + 1..];
                at_word_start = false;
                continue;
            }
        }
        if rest.starts_with('#') && at_word_start {
            let len = 1 + rest[1..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
                .map(char::len_utf8)
                .sum::<usize>();
            if len > 1 {
                flush(&mut out, &mut plain);
                out.push_str(&paint(true, Paint::Tag, &rest[..len]));
                rest = &rest[len..];
                at_word_start = false;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        plain.push(ch);
        at_word_start = ch == ' ' || ch == '(';
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut out, &mut plain);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_date_prefix_today() {
        let (p, rest) = parse_date_prefix("today: wrote the plan").unwrap();
        assert_eq!(p, DatePrefix::Today);
        assert_eq!(rest, "wrote the plan");
    }

    #[test]
    fn parse_date_prefix_yesterday_case_insensitive() {
        let (p, rest) = parse_date_prefix("Yesterday: had coffee").unwrap();
        assert_eq!(p, DatePrefix::Yesterday);
        assert_eq!(rest, "had coffee");
    }

    #[test]
    fn parse_date_prefix_iso() {
        let (p, rest) = parse_date_prefix("2026-05-15: backfilled").unwrap();
        let DatePrefix::Iso(d) = p else {
            panic!("expected Iso variant")
        };
        assert_eq!(d, jiff::civil::date(2026, 5, 15));
        assert_eq!(rest, "backfilled");
    }

    #[test]
    fn parse_date_prefix_no_match_when_no_colon() {
        assert!(parse_date_prefix("hello world").is_none());
    }

    #[test]
    fn parse_date_prefix_malformed_iso_returns_none() {
        assert!(parse_date_prefix("2026-13-99: nope").is_none());
        assert!(parse_date_prefix("totallynotadate: text").is_none());
    }

    #[test]
    fn parse_inline_attributes_extracts_time_and_starred() {
        let (attrs, body) =
            parse_inline_attributes("[time:: 14:32] [starred:: true] Did the thing");
        assert_eq!(attrs.get("time").map(String::as_str), Some("14:32"));
        assert_eq!(attrs.get("starred").map(String::as_str), Some("true"));
        assert_eq!(body, "Did the thing");
    }

    #[test]
    fn parse_inline_attributes_preserves_markdown_links() {
        let (attrs, body) =
            parse_inline_attributes("[time:: 09:00] See [docs](https://x.y) for context");
        assert_eq!(attrs.get("time").map(String::as_str), Some("09:00"));
        assert_eq!(body, "See [docs](https://x.y) for context");
    }

    #[test]
    fn parse_inline_attributes_ignores_bare_brackets() {
        let (attrs, body) = parse_inline_attributes("[just text] no attrs here");
        assert!(attrs.is_empty());
        assert_eq!(body, "[just text] no attrs here");
    }

    #[test]
    fn extract_tags_finds_hash_tags() {
        let tags = extract_tags("did stuff #engineering and #ops/oncall");
        assert_eq!(tags, vec!["engineering", "ops/oncall"]);
    }

    #[test]
    fn extract_tags_ignores_mid_word_hash() {
        let tags = extract_tags("issue#123 reference but #real-tag works");
        assert_eq!(tags, vec!["real-tag"]);
    }

    #[test]
    fn format_entry_single_line_with_time() {
        let s = format_entry(Some("14:32"), false, '*', "Wrote the plan", false, None);
        assert_eq!(s, "* [time:: 14:32] Wrote the plan");
    }

    #[test]
    fn format_entry_with_star() {
        let s = format_entry(Some("15:10"), true, '*', "Lunch with Sam", false, None);
        assert_eq!(s, "* [time:: 15:10] [starred:: true] Lunch with Sam");
    }

    #[test]
    fn format_entry_no_time() {
        let s = format_entry(None, false, '*', "Quick note", false, None);
        assert_eq!(s, "* Quick note");
    }

    #[test]
    fn format_entry_dash_bullet() {
        let s = format_entry(Some("08:00"), false, '-', "Morning standup", false, None);
        assert_eq!(s, "- [time:: 08:00] Morning standup");
    }

    #[test]
    fn format_entry_multi_line_uses_continuation_indent() {
        let s = format_entry(
            Some("16:45"),
            false,
            '*',
            "First line\nSecond line\nThird line",
            false,
            None,
        );
        assert_eq!(
            s,
            "* [time:: 16:45] First line\n  Second line\n  Third line"
        );
    }

    #[test]
    fn format_entry_task_with_tag() {
        // Checkbox sits immediately after the bullet; the tag is appended at end
        // of the first line so tag-gated task indexing picks it up.
        let s = format_entry(Some("14:32"), false, '*', "Buy milk", true, Some("task"));
        assert_eq!(s, "* [ ] [time:: 14:32] Buy milk #task");
    }

    #[test]
    fn format_entry_task_without_tag() {
        let s = format_entry(Some("14:32"), false, '*', "Buy milk", true, None);
        assert_eq!(s, "* [ ] [time:: 14:32] Buy milk");
    }

    #[test]
    fn format_entry_task_with_star_and_no_time() {
        let s = format_entry(None, true, '-', "Ship it", true, Some("todo"));
        assert_eq!(s, "- [ ] [starred:: true] Ship it #todo");
    }

    #[test]
    fn format_entry_tag_strips_leading_hash_and_ignores_empty() {
        // A user-supplied tag may include a leading '#'; it is normalized to one.
        let s = format_entry(None, false, '*', "x", true, Some("#urgent"));
        assert_eq!(s, "* [ ] x #urgent");
        // An empty/whitespace tag produces no trailing hash.
        let s = format_entry(None, false, '*', "x", true, Some("  "));
        assert_eq!(s, "* [ ] x");
    }

    #[test]
    fn format_entry_task_multi_line_tags_only_first_line() {
        let s = format_entry(None, false, '*', "line one\nline two", true, Some("task"));
        assert_eq!(s, "* [ ] line one #task\n  line two");
    }

    #[test]
    fn parse_entries_single_bullet() {
        let content = "* [time:: 14:32] Did the thing #work\n";
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(content, date, "Journal/2026-05-18");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.time.as_deref(), Some("14:32"));
        assert!(!e.starred);
        assert_eq!(e.body, "Did the thing #work");
        assert_eq!(e.tags, vec!["work"]);
    }

    #[test]
    fn parse_entries_multi_line_continuation() {
        let content = "* [time:: 16:45] First line\n  Second line\n  Third line\n";
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(content, date, "Journal/2026-05-18");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "First line\nSecond line\nThird line");
    }

    #[test]
    fn parse_entries_starred() {
        let content = "* [time:: 10:00] [starred:: true] Big win\n";
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(content, date, "Journal/2026-05-18");
        assert!(entries[0].starred);
    }

    #[test]
    fn parse_entries_ignores_non_bullet_lines() {
        let content = "# Header\n\nSome prose.\n\n* [time:: 09:00] entry\n";
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(content, date, "Journal/2026-05-18");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn parse_entries_ignores_nested_bullets() {
        let content = "* [time:: 09:00] parent\n  * indented child not its own entry\n";
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(content, date, "Journal/2026-05-18");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].body.contains("indented child"));
    }

    #[test]
    fn daily_dir_template_extracts_dir() {
        assert_eq!(daily_dir_template("Journal/{{date}}").unwrap(), "Journal");
        assert_eq!(daily_dir_template("{{date}}").unwrap(), "");
        assert_eq!(
            daily_dir_template("notes/daily/{{date}}").unwrap(),
            "notes/daily"
        );
    }

    #[test]
    fn daily_dir_template_rejects_trailing_suffix() {
        let err = daily_dir_template("Journal/{{date}}-notes").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("final path segment"), "got: {msg}");
    }

    #[test]
    fn daily_dir_template_requires_placeholder() {
        let err = daily_dir_template("Journal/fixed").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("{{date}}"), "got: {msg}");
    }

    #[test]
    fn parse_iso_date_valid() {
        let d = parse_iso_date("2026-05-18").unwrap();
        assert_eq!(d, jiff::civil::date(2026, 5, 18));
    }

    #[test]
    fn parse_iso_date_invalid() {
        for s in ["not-a-date", "2026/05/18"] {
            assert!(
                matches!(parse_iso_date(s), Err(SbError::Usage(_))),
                "{s:?} should be a usage error"
            );
        }
    }

    #[test]
    fn validate_time_hhmm_accepts_bounds_and_rejects_out_of_range() {
        for s in ["14:32", "00:00", "23:59"] {
            assert!(validate_time_hhmm(s).is_ok(), "{s:?} should be accepted");
        }
        for s in ["25:00", "12:60", "not-a-time"] {
            assert!(
                matches!(validate_time_hhmm(s), Err(SbError::Usage(_))),
                "{s:?} should be a usage error"
            );
        }
    }

    #[test]
    fn read_filters_date_window() {
        let f = ReadFilters {
            from: Some(Date::strptime("%Y-%m-%d", "2026-05-10").unwrap()),
            to: Some(Date::strptime("%Y-%m-%d", "2026-05-20").unwrap()),
            on: None,
            contains: None,
            tags: vec![],
            starred: false,
        };
        let mk = |s: &str| JournalEntry {
            date: Date::strptime("%Y-%m-%d", s).unwrap(),
            time: None,
            starred: false,
            tags: vec![],
            body: String::new(),
            attrs: BTreeMap::new(),
            source_page: String::new(),
        };
        assert!(f.matches(&mk("2026-05-15")));
        assert!(!f.matches(&mk("2026-05-09")));
        assert!(!f.matches(&mk("2026-05-21")));
    }

    #[test]
    fn read_filters_contains_is_case_insensitive() {
        let f = ReadFilters {
            from: None,
            to: None,
            on: None,
            contains: Some("MIGRATION".to_string()),
            tags: vec![],
            starred: false,
        };
        let e = JournalEntry {
            date: Date::strptime("%Y-%m-%d", "2026-05-15").unwrap(),
            time: None,
            starred: false,
            tags: vec![],
            body: "finished the migration spike".into(),
            attrs: BTreeMap::new(),
            source_page: String::new(),
        };
        assert!(f.matches(&e));
    }

    #[test]
    fn read_filters_tags_match_any() {
        let f = ReadFilters {
            from: None,
            to: None,
            on: None,
            contains: None,
            tags: vec!["engineering".into()],
            starred: false,
        };
        let mut e = JournalEntry {
            date: Date::strptime("%Y-%m-%d", "2026-05-15").unwrap(),
            time: None,
            starred: false,
            tags: vec!["ops".into()],
            body: String::new(),
            attrs: BTreeMap::new(),
            source_page: String::new(),
        };
        assert!(!f.matches(&e));
        e.tags.push("engineering".into());
        assert!(f.matches(&e));
    }

    #[test]
    fn read_filters_starred_only() {
        let f = ReadFilters {
            from: None,
            to: None,
            on: None,
            contains: None,
            tags: vec![],
            starred: true,
        };
        let mut e = JournalEntry {
            date: Date::strptime("%Y-%m-%d", "2026-05-15").unwrap(),
            time: None,
            starred: false,
            tags: vec![],
            body: String::new(),
            attrs: BTreeMap::new(),
            source_page: String::new(),
        };
        assert!(!f.matches(&e));
        e.starred = true;
        assert!(f.matches(&e));
    }

    #[test]
    fn list_daily_files_finds_matching_dates() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = tmp.path().join("Journal");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(journal.join("2026-05-15.md"), "").unwrap();
        std::fs::write(journal.join("2026-05-16.md"), "").unwrap();
        std::fs::write(journal.join("not-a-date.md"), "").unwrap();
        std::fs::write(journal.join("2026-05-17.txt"), "").unwrap();

        let mut files = list_daily_files(tmp.path(), "Journal", "%Y-%m-%d").unwrap();
        files.sort_by_key(|a| a.1);
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].1,
            Date::strptime("%Y-%m-%d", "2026-05-15").unwrap()
        );
        assert_eq!(
            files[1].1,
            Date::strptime("%Y-%m-%d", "2026-05-16").unwrap()
        );
    }

    #[test]
    fn resolve_daily_date_today_returns_today() {
        let today = jiff::Zoned::now().date();
        assert_eq!(resolve_daily_date(0).unwrap(), today);
    }

    #[test]
    fn resolve_daily_date_yesterday_is_one_day_before() {
        use jiff::ToSpan;
        let today = jiff::Zoned::now().date();
        let yesterday = today.checked_add((-1).days()).unwrap();
        assert_eq!(resolve_daily_date(-1).unwrap(), yesterday);
    }

    #[test]
    fn resolve_daily_date_positive_offset_is_in_future() {
        use jiff::ToSpan;
        let today = jiff::Zoned::now().date();
        let in_three = today.checked_add(3.days()).unwrap();
        assert_eq!(resolve_daily_date(3).unwrap(), in_three);
    }

    #[test]
    fn format_daily_path_replaces_date_placeholder() {
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        assert_eq!(
            format_daily_path("Journal/{{date}}", "%Y-%m-%d", &date),
            "Journal/2026-05-18"
        );
        assert_eq!(
            format_daily_path("daily-{{date}}-notes", "%Y/%m/%d", &date),
            "daily-2026/05/18-notes"
        );
    }

    #[test]
    fn parse_inline_attributes_handles_unicode_body() {
        let (attrs, body) = parse_inline_attributes("[time:: 09:00] こんにちは 🌸 done");
        assert_eq!(attrs.get("time").map(String::as_str), Some("09:00"));
        assert_eq!(body, "こんにちは 🌸 done");
    }

    #[test]
    fn parse_entries_dash_bullets() {
        let content = "- [time:: 08:00] dash entry one\n- [time:: 08:30] dash entry two\n";
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(content, date, "Journal/2026-05-18");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].body, "dash entry one");
        assert_eq!(entries[1].time.as_deref(), Some("08:30"));
    }

    #[test]
    fn parse_entries_collects_arbitrary_attributes() {
        let content =
            "* [time:: 14:00] [project:: phoenix] [priority:: P1] launch readiness review\n";
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(content, date, "Journal/2026-05-18");
        assert_eq!(
            entries[0].attrs.get("project").map(String::as_str),
            Some("phoenix")
        );
        assert_eq!(
            entries[0].attrs.get("priority").map(String::as_str),
            Some("P1")
        );
        assert_eq!(entries[0].body, "launch readiness review");
    }

    #[test]
    fn list_daily_files_returns_empty_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let files = list_daily_files(tmp.path(), "DoesNotExist", "%Y-%m-%d").unwrap();
        assert!(files.is_empty());
    }

    // --- append_entry: file-IO behaviour ---

    fn write_test_file(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn append_entry_to_empty_file_has_no_leading_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_file(tmp.path(), "day.md", "");
        append_entry(&p, "* [time:: 09:00] hello", false).unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, "* [time:: 09:00] hello\n");
    }

    #[test]
    fn append_entry_to_file_without_trailing_newline_prepends_one() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_file(tmp.path(), "day.md", "# Header");
        append_entry(&p, "* [time:: 09:00] hello", false).unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, "# Header\n* [time:: 09:00] hello\n");
    }

    #[test]
    fn append_entry_to_file_with_trailing_newline_no_extra_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_file(tmp.path(), "day.md", "# Header\n");
        append_entry(&p, "* [time:: 09:00] hello", false).unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, "# Header\n* [time:: 09:00] hello\n");
    }

    #[test]
    fn append_entry_multi_line_continuation_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_file(tmp.path(), "day.md", "* [time:: 08:00] first\n");
        let multi = format_entry(Some("09:00"), false, '*', "line one\nline two", false, None);
        append_entry(&p, &multi, false).unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            got,
            "* [time:: 08:00] first\n* [time:: 09:00] line one\n  line two\n"
        );
        // Round-trip: parse the file back and confirm two entries with intact bodies
        let date = Date::strptime("%Y-%m-%d", "2026-05-18").unwrap();
        let entries = parse_entries(&got, date, "Journal/2026-05-18");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].body, "line one\nline two");
    }

    // --- ensure_day_file: file creation ---

    fn minimal_config(space_root: &std::path::Path) -> ResolvedConfig {
        // Build a config that doesn't reference any template — avoids network.
        std::fs::create_dir_all(space_root.join(".sb")).unwrap();
        std::fs::write(
            space_root.join(".sb").join("config.toml"),
            r#"[daily]
path = "Journal/{{date}}"
dateFormat = "%Y-%m-%d"
"#,
        )
        .unwrap();
        ResolvedConfig::load_from(space_root).unwrap()
    }

    #[tokio::test]
    async fn ensure_day_file_creates_when_missing_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let page_path = tmp.path().join("Journal").join("2026-05-18.md");
        let created = ensure_day_file(
            &page_path,
            tmp.path(),
            &config,
            None,
            &Date::constant(2026, 5, 18),
        )
        .await
        .unwrap();
        assert!(created);
        assert!(page_path.exists());
        assert_eq!(std::fs::read_to_string(&page_path).unwrap(), "");
    }

    #[tokio::test]
    async fn ensure_day_file_returns_false_when_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let dir = tmp.path().join("Journal");
        std::fs::create_dir_all(&dir).unwrap();
        let page_path = dir.join("2026-05-18.md");
        std::fs::write(&page_path, "existing content").unwrap();
        let created = ensure_day_file(
            &page_path,
            tmp.path(),
            &config,
            None,
            &Date::constant(2026, 5, 18),
        )
        .await
        .unwrap();
        assert!(!created);
        // Untouched
        assert_eq!(
            std::fs::read_to_string(&page_path).unwrap(),
            "existing content"
        );
    }

    // --- render_entries: capture output via a Vec<u8> writer ---

    fn sample_entry(date: &str, time: Option<&str>, body: &str, starred: bool) -> JournalEntry {
        JournalEntry {
            date: Date::strptime("%Y-%m-%d", date).unwrap(),
            time: time.map(String::from),
            starred,
            tags: vec![],
            body: body.to_string(),
            attrs: BTreeMap::new(),
            source_page: format!("Journal/{}", date),
        }
    }

    #[test]
    fn render_entries_human_long_form() {
        let entries = vec![sample_entry(
            "2026-05-18",
            Some("14:32"),
            "Finished the migration spike",
            false,
        )];
        let mut buf = Vec::new();
        render_entries(&entries, &OutputFormat::Human, false, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(
            s,
            "Monday 2026-05-18\n  14:32  Finished the migration spike\n\n"
        );
    }

    #[test]
    fn render_entries_human_groups_by_date_and_splits_signature_and_attrs() {
        let mut e = sample_entry(
            "2026-05-18",
            Some("09:00"),
            "Swab done\n\nPara one.\n\nPara two. -- @barbossa",
            false,
        );
        e.attrs.insert("time".into(), "09:00".into());
        e.attrs.insert("result".into(), "done".into());
        let entries = vec![
            e,
            sample_entry("2026-05-18", Some("08:00"), "same day", false),
            sample_entry("2026-05-17", None, "Day summary: quiet", true),
        ];
        let mut buf = Vec::new();
        render_entries(&entries, &OutputFormat::Human, false, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(
            s,
            "Monday 2026-05-18\n\
             \x20 09:00  Swab done\n\
             \x20        result: done\n\
             \n\
             \x20        Para one.\n\
             \n\
             \x20        Para two.\n\
             \x20        -- @barbossa\n\
             \n\
             \x20 08:00  same day\n\
             \n\
             \n\
             Sunday 2026-05-17\n\
             \x20 --:--  ★ Day summary: quiet\n\
             \n"
        );
    }

    #[test]
    fn render_entries_styled_wraps_body_to_width_with_hanging_list_indent() {
        let body = "Head\nalpha beta gamma delta epsilon\n- one two three four five six";
        let entries = vec![sample_entry("2026-05-18", Some("09:00"), body, false)];
        let mut buf = Vec::new();
        let style = RenderStyle {
            color: false,
            width: Some(9 + 20),
        };
        render_entries_styled(&entries, &OutputFormat::Human, false, style, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let body_lines: Vec<&str> = s.lines().skip(2).filter(|l| !l.is_empty()).collect();
        assert_eq!(
            body_lines,
            vec![
                "         alpha beta gamma",
                "         delta epsilon",
                "         - one two three four",
                "           five six",
            ]
        );
        assert!(s.lines().all(|l| console::measure_text_width(l) <= 29));
    }

    #[test]
    fn split_signature_only_takes_a_trailing_mention_list() {
        assert_eq!(
            split_signature("text -- @a @b"),
            ("text".to_string(), Some("@a @b".to_string()))
        );
        assert_eq!(
            split_signature("line\n-- @cam"),
            ("line".to_string(), Some("@cam".to_string()))
        );
        // Not a signature: prose after the mention, or no @ at all.
        assert_eq!(split_signature("a -- @b said hi").1, None);
        assert_eq!(split_signature("x -- y").1, None);
    }

    #[test]
    fn json_entries_carry_inline_attributes() {
        let mut e = sample_entry("2026-05-18", Some("09:00"), "x", false);
        e.attrs.insert("result".into(), "blocked".into());
        let mut buf = Vec::new();
        render_entries(&[e], &OutputFormat::Json, false, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v[0]["attrs"]["result"], "blocked");
    }

    #[test]
    fn parse_entries_keeps_paragraph_breaks_inside_one_entry() {
        let content =
            "* [time:: 09:00] Head\n\n  Para one.\n\n  Para two.\n\n* [time:: 10:00] Next\n";
        let d = Date::constant(2026, 5, 18);
        let entries = parse_entries(content, d, "J/2026-05-18");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].body, "Head\n\nPara one.\n\nPara two.");
        assert_eq!(entries[1].body, "Next");
    }

    #[test]
    fn parse_entries_blank_line_then_unindented_text_ends_the_entry() {
        let content = "* one\n\nA heading paragraph\n  not a continuation\n";
        let entries = parse_entries(content, Date::constant(2026, 5, 18), "J");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "one");
    }

    #[test]
    fn format_entry_writes_no_whitespace_only_lines() {
        let s = format_entry(Some("09:00"), false, '*', "Head\n\nBody", false, None);
        assert_eq!(s, "* [time:: 09:00] Head\n\n  Body");
    }

    #[test]
    fn append_entry_with_spacing_separates_entries_by_one_blank_line() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("d.md");
        std::fs::write(&p, "").unwrap();
        append_entry(&p, "* one", true).unwrap();
        append_entry(&p, "* two", true).unwrap();
        std::fs::write(&p, std::fs::read_to_string(&p).unwrap() + "\n").unwrap(); // already blank
        append_entry(&p, "* three", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "* one\n\n* two\n\n* three\n"
        );
    }

    #[test]
    fn append_entry_spacing_counts_crlf_and_whitespace_blank_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("d.md");
        for (start, want) in [
            ("x\r\n\r\n", "x\r\n\r\n* n\n"),
            ("x\n  \n", "x\n  \n* n\n"),
            ("x\r\n", "x\r\n\n* n\n"),
            ("x", "x\n\n* n\n"),
        ] {
            std::fs::write(&p, start).unwrap();
            append_entry(&p, "* n", true).unwrap();
            assert_eq!(
                std::fs::read_to_string(&p).unwrap(),
                want,
                "start={start:?}"
            );
        }
    }

    #[test]
    fn starred_headline_respects_wrap_width() {
        let head = "word ".repeat(12);
        let entries = vec![sample_entry("2026-05-18", Some("09:00"), head.trim(), true)];
        let mut buf = Vec::new();
        let style = RenderStyle {
            color: false,
            width: Some(40),
        };
        render_entries_styled(&entries, &OutputFormat::Human, false, style, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.lines().all(|l| console::measure_text_width(l) <= 40),
            "{s}"
        );
    }

    #[tokio::test]
    async fn daily_plain_template_leaves_dollar_braces_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let mut config = minimal_config(tmp.path());
        config.runtime_available.value = true; // must not be consulted for a plain file
        let raw = "echo ${HOME} on {{date}}\n";
        let out = render_daily_template(raw, &Date::constant(2026, 11, 17), &config, None).await;
        assert_eq!(out, "echo ${HOME} on 2026-11-17\n");
    }

    #[test]
    fn fill_date_placeholders_uses_the_notes_date_and_format() {
        let d = Date::constant(2026, 11, 17);
        assert_eq!(
            fill_date_placeholders("# Log {{date}} ({{weekday}})", &d, "%d.%m.%Y"),
            "# Log 17.11.2026 (Tuesday)"
        );
    }

    #[tokio::test]
    async fn daily_page_template_drops_its_own_frontmatter_and_fills_the_date() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let raw =
            "---\ntags: meta/template/page\nfrontmatter: |\n  tags: log\n---\n# Log {{date}}\n";
        let out = render_daily_template(raw, &Date::constant(2026, 11, 17), &config, None).await;
        assert_eq!(out, "---\ntags: log\n---\n\n# Log 2026-11-17\n");
    }

    #[tokio::test]
    async fn daily_plain_template_keeps_its_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let raw = "---\ntags: journal\n---\n# {{date}}\n";
        let out = render_daily_template(raw, &Date::constant(2026, 11, 17), &config, None).await;
        assert_eq!(out, "---\ntags: journal\n---\n# 2026-11-17\n");
    }

    #[test]
    fn render_entries_short_truncates_long_bodies() {
        let body: String = "x".repeat(120);
        let entries = vec![sample_entry("2026-05-18", Some("09:00"), &body, false)];
        let mut buf = Vec::new();
        render_entries(&entries, &OutputFormat::Human, true, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Should contain ellipsis and not the full body
        assert!(s.contains('…'));
        assert!(!s.contains(&"x".repeat(120)));
    }

    #[test]
    fn render_entries_short_renders_first_line_only() {
        let entries = vec![sample_entry(
            "2026-05-18",
            Some("09:00"),
            "first line\nsecond line",
            false,
        )];
        let mut buf = Vec::new();
        render_entries(&entries, &OutputFormat::Human, true, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("first line"));
        assert!(!s.contains("second line"));
    }

    #[test]
    fn render_entries_starred_shows_star_marker() {
        let entries = vec![sample_entry("2026-05-18", Some("09:00"), "big win", true)];
        let mut buf = Vec::new();
        render_entries(&entries, &OutputFormat::Human, true, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('★'));
    }

    #[test]
    fn render_entries_missing_time_renders_placeholder() {
        let entries = vec![sample_entry("2026-05-18", None, "no time", false)];
        let mut buf = Vec::new();
        render_entries(&entries, &OutputFormat::Human, true, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("--:--"));
    }

    #[test]
    fn render_entries_json_is_valid_array() {
        let entries = vec![
            sample_entry("2026-05-18", Some("14:32"), "first", false),
            sample_entry("2026-05-17", Some("10:00"), "second", true),
        ];
        let mut buf = Vec::new();
        render_entries(&entries, &OutputFormat::Json, false, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["date"], "2026-05-18");
        assert_eq!(arr[0]["time"], "14:32");
        assert_eq!(arr[0]["starred"], false);
        assert_eq!(arr[1]["body"], "second");
        assert_eq!(arr[1]["starred"], true);
    }

    #[test]
    fn render_entries_empty_list_human_is_empty_output() {
        let mut buf = Vec::new();
        render_entries(&[], &OutputFormat::Human, false, &mut buf).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn render_entries_empty_list_json_is_empty_array() {
        let mut buf = Vec::new();
        render_entries(&[], &OutputFormat::Json, false, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 0);
    }

    // --- end-to-end orchestration: collect_entries through a real space ---

    fn seed_journal(space_root: &std::path::Path, name: &str, contents: &str) {
        let journal = space_root.join("Journal");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(journal.join(name), contents).unwrap();
    }

    fn default_args<'a>(format: &'a OutputFormat) -> DailyArgs<'a> {
        DailyArgs {
            cli_token: None,
            entry: vec![],
            yesterday: false,
            offset: None,
            on: None,
            star: false,
            time: None,
            no_time: false,
            task: false,
            task_tag: None,
            no_task_tag: false,
            append: None,
            sign: &[],
            limit: None,
            from: None,
            to: None,
            contains: None,
            tags: vec![],
            starred: false,
            short: false,
            format,
            quiet: true,
            color: false,
        }
    }

    #[test]
    fn collect_entries_returns_sorted_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        seed_journal(tmp.path(), "2026-05-15.md", "* [time:: 09:00] earliest\n");
        seed_journal(
            tmp.path(),
            "2026-05-18.md",
            "* [time:: 09:00] today-morning\n* [time:: 17:00] today-evening\n",
        );

        let fmt = OutputFormat::Json;
        let args = default_args(&fmt);
        let got = collect_entries(tmp.path(), &config, &args).unwrap();

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].body, "today-evening");
        assert_eq!(got[1].body, "today-morning");
        assert_eq!(got[2].body, "earliest");
    }

    /// Seed one daily file per day for today-3..=today with the body `day-N`
    /// (N = days back). Midnight caveat: the production code reads the wall
    /// clock itself, so a run straddling local midnight can see a shifted
    /// "today" (same limitation as the other daily tests).
    /// Seeds today and the three days before it; returns the "today" it used.
    fn seed_last_days(space_root: &std::path::Path) -> jiff::civil::Date {
        use jiff::ToSpan;
        let today = jiff::Zoned::now().date();
        for back in 0..=3i32 {
            let d = today.checked_add((-back).days()).unwrap();
            seed_journal(
                space_root,
                &format!("{}.md", d.strftime("%Y-%m-%d")),
                &format!("* [time:: 09:00] day-{back}\n"),
            );
        }
        today
    }

    /// collect_entries reads the clock itself. If midnight passed between
    /// seeding and reading, the result is inconclusive, not wrong.
    fn crossed_midnight(seeded: jiff::civil::Date) -> bool {
        jiff::Zoned::now().date() != seeded
    }

    #[test]
    fn collect_entries_yesterday_reads_only_yesterdays_file() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let seeded = seed_last_days(tmp.path());

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.yesterday = true;
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        if crossed_midnight(seeded) {
            return;
        }
        let bodies: Vec<&str> = got.iter().map(|e| e.body.as_str()).collect();
        assert_eq!(bodies, vec!["day-1"]);
    }

    #[test]
    fn collect_entries_negative_offset_reads_that_days_file() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let seeded = seed_last_days(tmp.path());

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.offset = Some(-3);
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        args.offset = Some(0);
        let got_today = collect_entries(tmp.path(), &config, &args).unwrap();
        if crossed_midnight(seeded) {
            return;
        }
        let bodies: Vec<&str> = got.iter().map(|e| e.body.as_str()).collect();
        assert_eq!(bodies, vec!["day-3"]);

        let got = got_today;
        let bodies: Vec<&str> = got.iter().map(|e| e.body.as_str()).collect();
        assert_eq!(bodies, vec!["day-0"]);
    }

    #[test]
    fn collect_entries_applies_limit() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        seed_journal(
            tmp.path(),
            "2026-05-18.md",
            "* [time:: 09:00] a\n* [time:: 10:00] b\n* [time:: 11:00] c\n",
        );

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.limit = Some(2);
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, "c");
        assert_eq!(got[1].body, "b");
    }

    #[test]
    fn collect_entries_filters_by_on_flag() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        seed_journal(tmp.path(), "2026-05-15.md", "* [time:: 09:00] older\n");
        seed_journal(tmp.path(), "2026-05-18.md", "* [time:: 09:00] today\n");

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.on = Some("2026-05-15");
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "older");
    }

    #[test]
    fn collect_entries_filters_by_contains() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        seed_journal(
            tmp.path(),
            "2026-05-18.md",
            "* [time:: 09:00] migration spike\n* [time:: 10:00] something else\n",
        );

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.contains = Some("migration");
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].body.contains("migration"));
    }

    #[test]
    fn collect_entries_filters_by_tags() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        seed_journal(
            tmp.path(),
            "2026-05-18.md",
            "* [time:: 09:00] one #engineering\n* [time:: 10:00] two #ops\n",
        );

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.tags = vec!["engineering".to_string()];
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].tags.contains(&"engineering".to_string()));
    }

    #[test]
    fn collect_entries_filters_by_starred() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        seed_journal(
            tmp.path(),
            "2026-05-18.md",
            "* [time:: 09:00] [starred:: true] win\n* [time:: 10:00] normal\n",
        );

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.starred = true;
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].starred);
    }

    #[test]
    fn collect_entries_positional_acts_as_contains() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        seed_journal(
            tmp.path(),
            "2026-05-18.md",
            "* [time:: 09:00] migration spike\n* [time:: 10:00] coffee\n",
        );

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.entry = vec!["migration".to_string()];
        let got = collect_entries(tmp.path(), &config, &args).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].body.contains("migration"));
    }

    // --- end-to-end orchestration: execute_write_or_editor through a real space ---

    #[tokio::test]
    async fn execute_write_appends_entry_with_time_attr() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["Finished".into(), "the".into(), "spike".into()];
        args.time = Some("14:32");

        super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap();

        let today = jiff::Zoned::now().date().strftime("%Y-%m-%d").to_string();
        let written =
            std::fs::read_to_string(tmp.path().join("Journal").join(format!("{}.md", today)))
                .unwrap();
        assert_eq!(written, "* [time:: 14:32] Finished the spike\n");
    }

    #[tokio::test]
    async fn execute_write_routes_via_date_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["2026-05-15:".into(), "backfilled".into()];
        args.time = Some("09:00");

        super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap();

        let written =
            std::fs::read_to_string(tmp.path().join("Journal").join("2026-05-15.md")).unwrap();
        assert_eq!(written, "* [time:: 09:00] backfilled\n");
    }

    #[tokio::test]
    async fn execute_write_with_star_writes_starred_attr() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["big".into(), "win".into()];
        args.time = Some("10:00");
        args.star = true;

        super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap();

        let today = jiff::Zoned::now().date().strftime("%Y-%m-%d").to_string();
        let written =
            std::fs::read_to_string(tmp.path().join("Journal").join(format!("{}.md", today)))
                .unwrap();
        assert!(written.contains("[starred:: true]"));
    }

    #[tokio::test]
    async fn execute_write_with_no_time_omits_time_attr() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["bare".into(), "entry".into()];
        args.no_time = true;

        super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap();

        let today = jiff::Zoned::now().date().strftime("%Y-%m-%d").to_string();
        let written =
            std::fs::read_to_string(tmp.path().join("Journal").join(format!("{}.md", today)))
                .unwrap();
        assert_eq!(written, "* bare entry\n");
    }

    #[tokio::test]
    async fn execute_write_prefix_conflict_with_flag_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["today:".into(), "stuff".into()];
        args.yesterday = true;

        let err = super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap_err();
        match err {
            SbError::Usage(msg) => assert!(msg.contains("date prefix")),
            other => panic!("expected Usage error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_write_invalid_time_flag_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["entry".into()];
        args.time = Some("not-a-time");

        let err = super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::Usage(_)));
    }

    #[tokio::test]
    async fn execute_write_invalid_on_flag_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["entry".into()];
        args.on = Some("not-a-date");

        let err = super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::Usage(_)));
    }

    #[tokio::test]
    async fn execute_write_legacy_append_flag_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.append = Some("legacy entry");
        args.time = Some("09:00");

        super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap();

        let today = jiff::Zoned::now().date().strftime("%Y-%m-%d").to_string();
        let written =
            std::fs::read_to_string(tmp.path().join("Journal").join(format!("{}.md", today)))
                .unwrap();
        assert_eq!(written, "* [time:: 09:00] legacy entry\n");
    }

    #[tokio::test]
    async fn fetch_template_content_returns_none_with_no_server_and_no_local() {
        let tmp = tempfile::tempdir().unwrap();
        // Strips SB_* (SB_SERVER_URL/SB_TOKEN would send template fetches to
        // the developer's real server) and isolates XDG, under the env lock.
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = minimal_config(tmp.path());
        let got = super::fetch_template_content(tmp.path(), "Missing", &config, None)
            .await
            .unwrap();
        assert!(got.is_none());
    }

    // --- task tag resolution ---

    /// Build a config with explicit task-tag settings (no network involved).
    fn task_config(space_root: &std::path::Path, mode: &str, tag: &str) -> ResolvedConfig {
        std::fs::create_dir_all(space_root.join(".sb")).unwrap();
        std::fs::write(
            space_root.join(".sb").join("config.toml"),
            format!(
                "[daily]\npath = \"Journal/{{{{date}}}}\"\ntaskTagMode = \"{mode}\"\ntaskTag = \"{tag}\"\n"
            ),
        )
        .unwrap();
        ResolvedConfig::load_from(space_root).unwrap()
    }

    #[tokio::test]
    async fn resolve_task_tag_no_task_tag_flag_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = task_config(tmp.path(), "always", "task");
        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.task = true;
        args.no_task_tag = true;
        assert_eq!(resolve_task_tag(&config, &args).await, None);
    }

    #[tokio::test]
    async fn resolve_task_tag_explicit_flag_overrides_config() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = task_config(tmp.path(), "never", "task");
        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.task = true;
        args.task_tag = Some("urgent");
        assert_eq!(
            resolve_task_tag(&config, &args).await,
            Some("urgent".to_string())
        );
    }

    #[tokio::test]
    async fn resolve_task_tag_mode_never_and_always() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.task = true;

        let never = task_config(tmp.path(), "never", "task");
        assert_eq!(resolve_task_tag(&never, &args).await, None);

        let always = task_config(tmp.path(), "always", "todo");
        assert_eq!(
            resolve_task_tag(&always, &args).await,
            Some("todo".to_string())
        );
    }

    #[tokio::test]
    async fn resolve_task_tag_auto_tags_when_index_task_all_false() {
        use crate::test_util::{make_space, SbSpaceGuard};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":false}"#))
            .mount(&server)
            .await;

        let tmp = make_space(Some(&server.uri()));
        crate::config::update_config_value(&tmp.path().join(".sb"), "daily", "taskTagMode", "auto")
            .unwrap();
        crate::config::update_config_value(&tmp.path().join(".sb"), "runtime", "available", true)
            .unwrap();
        // Guard first: loading config reads the env the guard scrubs.
        let _g = SbSpaceGuard::set(tmp.path());
        let config = ResolvedConfig::load_from(tmp.path()).unwrap();

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.task = true;
        // index.task.all == false => only tagged tasks are indexed => apply the tag.
        assert_eq!(
            resolve_task_tag(&config, &args).await,
            Some("task".to_string())
        );
    }

    #[tokio::test]
    async fn resolve_task_tag_auto_no_tag_when_index_task_all_true() {
        use crate::test_util::{make_space, SbSpaceGuard};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":true}"#))
            .mount(&server)
            .await;

        let tmp = make_space(Some(&server.uri()));
        crate::config::update_config_value(&tmp.path().join(".sb"), "daily", "taskTagMode", "auto")
            .unwrap();
        crate::config::update_config_value(&tmp.path().join(".sb"), "runtime", "available", true)
            .unwrap();
        // Guard first: loading config reads the env the guard scrubs.
        let _g = SbSpaceGuard::set(tmp.path());
        let config = ResolvedConfig::load_from(tmp.path()).unwrap();

        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.task = true;
        // index.task.all == true => all tasks indexed => leave it bare.
        assert_eq!(resolve_task_tag(&config, &args).await, None);
    }

    #[tokio::test]
    async fn resolve_task_tag_auto_falls_back_to_tag_when_runtime_unavailable() {
        // runtime.available defaults to false; no network is attempted and we
        // fall back to tagging so the task is indexed under either setting.
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::test_util::SbSpaceGuard::set(tmp.path());
        let config = task_config(tmp.path(), "auto", "task");
        let fmt = OutputFormat::Json;
        let mut args = default_args(&fmt);
        args.task = true;
        assert_eq!(
            resolve_task_tag(&config, &args).await,
            Some("task".to_string())
        );
    }
}
