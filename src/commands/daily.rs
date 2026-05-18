use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use jiff::civil::Date;

use crate::cli::OutputFormat;
use crate::commands::page::{find_space_root, open_in_editor, validate_page_path};
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
/// Multi-line bodies use 2-space continuation indentation so the whole entry
/// remains a single bullet item.
pub fn format_entry(time: Option<&str>, star: bool, bullet: char, body: &str) -> String {
    let body = body.trim();
    let mut prefix = String::new();
    if let Some(t) = time {
        prefix.push_str(&format!("[time:: {}] ", t));
    }
    if star {
        prefix.push_str("[starred:: true] ");
    }
    let mut lines = body.lines();
    let first = lines.next().unwrap_or("");
    let mut out = format!("{} {}{}", bullet, prefix, first);
    for line in lines {
        out.push('\n');
        out.push_str("  ");
        out.push_str(line);
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
/// same bullet. Anything else terminates the bullet.
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
                if let Some(rest) = next.strip_prefix("  ") {
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

/// Fetch template content: try local file first, then remote.
///
/// Returns `None` when the template cannot be found in either location.
async fn fetch_template_content(
    space_root: &Path,
    template_name: &str,
    config: &ResolvedConfig,
    cli_token: Option<&str>,
) -> SbResult<Option<String>> {
    let local_path = space_root.join(format!("{}.md", template_name));
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
                let client = crate::client::SbClient::new(url, &token)?;
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

/// Ensure the day's note file exists. Creates parent dirs and applies the
/// configured template if the file is being created for the first time.
/// Returns `true` when the file was just created.
async fn ensure_day_file(
    page_path: &Path,
    space_root: &Path,
    config: &ResolvedConfig,
    cli_token: Option<&str>,
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
    let content = if let Some(ref template_name) = config.daily_template.value {
        fetch_template_content(space_root, template_name, config, cli_token)
            .await?
            .unwrap_or_default()
    } else {
        String::new()
    };
    std::fs::write(page_path, &content).map_err(|e| SbError::Filesystem {
        message: "failed to create daily note".into(),
        path: page_path.display().to_string(),
        source: Some(e),
    })?;
    Ok(true)
}

/// Append a formatted entry to the day's note, prefixing with a newline when
/// the file already has trailing content.
fn append_entry(page_path: &Path, entry_md: &str) -> SbResult<()> {
    let needs_leading_newline = std::fs::metadata(page_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
        && std::fs::read(page_path)
            .map(|b| b.last().copied() != Some(b'\n'))
            .unwrap_or(true);

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(page_path)
        .map_err(|e| SbError::Filesystem {
            message: "failed to open daily note for append".into(),
            path: page_path.display().to_string(),
            source: Some(e),
        })?;

    let prefix = if needs_leading_newline { "\n" } else { "" };
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
    pub append: Option<&'a str>,
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
        return execute_read(&space_root, &config, &args).await;
    }

    execute_write_or_editor(&space_root, &config, &args, stdin_piped).await
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

    if entry_text.is_empty() {
        let created = ensure_day_file(&page_path, space_root, config, args.cli_token).await?;
        if created {
            output::print_success(&format!("Created {}", page_name), args.color, args.quiet);
        }
        return open_in_editor(&page_path).await;
    }

    let created = ensure_day_file(&page_path, space_root, config, args.cli_token).await?;
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
    let entry_md = format_entry(time_str.as_deref(), args.star, bullet_char, &entry_text);

    append_entry(&page_path, &entry_md)?;
    output::print_success(
        &format!("Appended to {}", page_name),
        args.color,
        args.quiet,
    );
    Ok(())
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
        if self.starred && !e.starred {
            return false;
        }
        true
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

    entries.sort_by(|a, b| {
        b.date
            .cmp(&a.date)
            .then_with(|| b.time.cmp(&a.time))
            .then_with(|| a.body.cmp(&b.body))
    });

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
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_entries(&entries, args.format, args.short, &mut handle)
}

pub fn render_entries(
    entries: &[JournalEntry],
    format: &OutputFormat,
    short: bool,
    out: &mut dyn Write,
) -> SbResult<()> {
    match format {
        OutputFormat::Json => {
            #[derive(serde::Serialize)]
            struct JsonEntry<'a> {
                date: String,
                time: Option<&'a str>,
                starred: bool,
                tags: &'a [String],
                body: &'a str,
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
                    source_page: &e.source_page,
                })
                .collect();
            let s = serde_json::to_string_pretty(&json).map_err(|e| SbError::Config {
                message: format!("failed to serialize entries: {e}"),
            })?;
            writeln!(out, "{}", s).map_err(|e| SbError::Filesystem {
                message: "failed to write entries".into(),
                path: "<stdout>".into(),
                source: Some(e),
            })?;
        }
        OutputFormat::Human => {
            for e in entries {
                let date_str = e.date.strftime("%Y-%m-%d").to_string();
                let time_str = e.time.as_deref().unwrap_or("--:--");
                let star = if e.starred { "★ " } else { "" };
                let write_err = |e: std::io::Error| SbError::Filesystem {
                    message: "failed to write entry".into(),
                    path: "<stdout>".into(),
                    source: Some(e),
                };
                if short {
                    let first = e.body.lines().next().unwrap_or("");
                    let truncated: String = if first.chars().count() > 80 {
                        let mut s: String = first.chars().take(79).collect();
                        s.push('…');
                        s
                    } else {
                        first.to_string()
                    };
                    writeln!(out, "{} {}  {}{}", date_str, time_str, star, truncated)
                        .map_err(write_err)?;
                } else {
                    writeln!(out, "{} {}  {}", date_str, time_str, star).map_err(write_err)?;
                    for line in e.body.lines() {
                        writeln!(out, "    {}", line).map_err(write_err)?;
                    }
                    writeln!(out).map_err(write_err)?;
                }
            }
        }
    }
    Ok(())
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
        assert_eq!(d.year(), 2026);
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
        let s = format_entry(Some("14:32"), false, '*', "Wrote the plan");
        assert_eq!(s, "* [time:: 14:32] Wrote the plan");
    }

    #[test]
    fn format_entry_with_star() {
        let s = format_entry(Some("15:10"), true, '*', "Lunch with Sam");
        assert_eq!(s, "* [time:: 15:10] [starred:: true] Lunch with Sam");
    }

    #[test]
    fn format_entry_no_time() {
        let s = format_entry(None, false, '*', "Quick note");
        assert_eq!(s, "* Quick note");
    }

    #[test]
    fn format_entry_dash_bullet() {
        let s = format_entry(Some("08:00"), false, '-', "Morning standup");
        assert_eq!(s, "- [time:: 08:00] Morning standup");
    }

    #[test]
    fn format_entry_multi_line_uses_continuation_indent() {
        let s = format_entry(
            Some("16:45"),
            false,
            '*',
            "First line\nSecond line\nThird line",
        );
        assert_eq!(
            s,
            "* [time:: 16:45] First line\n  Second line\n  Third line"
        );
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
        assert_eq!(d.year(), 2026);
    }

    #[test]
    fn parse_iso_date_invalid() {
        assert!(parse_iso_date("not-a-date").is_err());
        assert!(parse_iso_date("2026/05/18").is_err());
    }

    #[test]
    fn validate_time_hhmm_accepts_valid() {
        assert!(validate_time_hhmm("14:32").is_ok());
        assert!(validate_time_hhmm("00:00").is_ok());
        assert!(validate_time_hhmm("23:59").is_ok());
    }

    #[test]
    fn validate_time_hhmm_rejects_invalid() {
        assert!(validate_time_hhmm("25:00").is_err());
        assert!(validate_time_hhmm("not-a-time").is_err());
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
        append_entry(&p, "* [time:: 09:00] hello").unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, "* [time:: 09:00] hello\n");
    }

    #[test]
    fn append_entry_to_file_without_trailing_newline_prepends_one() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_file(tmp.path(), "day.md", "# Header");
        append_entry(&p, "* [time:: 09:00] hello").unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, "# Header\n* [time:: 09:00] hello\n");
    }

    #[test]
    fn append_entry_to_file_with_trailing_newline_no_extra_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_file(tmp.path(), "day.md", "# Header\n");
        append_entry(&p, "* [time:: 09:00] hello").unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, "# Header\n* [time:: 09:00] hello\n");
    }

    #[test]
    fn append_entry_multi_line_continuation_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_test_file(tmp.path(), "day.md", "* [time:: 08:00] first\n");
        let multi = format_entry(Some("09:00"), false, '*', "line one\nline two");
        append_entry(&p, &multi).unwrap();
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
        let config = minimal_config(tmp.path());
        let page_path = tmp.path().join("Journal").join("2026-05-18.md");
        let created = ensure_day_file(&page_path, tmp.path(), &config, None)
            .await
            .unwrap();
        assert!(created);
        assert!(page_path.exists());
        assert_eq!(std::fs::read_to_string(&page_path).unwrap(), "");
    }

    #[tokio::test]
    async fn ensure_day_file_returns_false_when_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let config = minimal_config(tmp.path());
        let dir = tmp.path().join("Journal");
        std::fs::create_dir_all(&dir).unwrap();
        let page_path = dir.join("2026-05-18.md");
        std::fs::write(&page_path, "existing content").unwrap();
        let created = ensure_day_file(&page_path, tmp.path(), &config, None)
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
        assert!(s.contains("2026-05-18 14:32"));
        assert!(s.contains("    Finished the migration spike"));
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
            append: None,
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

    #[test]
    fn collect_entries_applies_limit() {
        let tmp = tempfile::tempdir().unwrap();
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
    async fn execute_write_appends_to_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = minimal_config(tmp.path());
        seed_journal(
            tmp.path(),
            &format!("{}.md", jiff::Zoned::now().date().strftime("%Y-%m-%d")),
            "* [time:: 08:00] existing\n",
        );
        let fmt = OutputFormat::Human;
        let mut args = default_args(&fmt);
        args.entry = vec!["new".into(), "entry".into()];
        args.time = Some("10:00");

        super::execute_write_or_editor(tmp.path(), &config, &args, false)
            .await
            .unwrap();

        let today = jiff::Zoned::now().date().strftime("%Y-%m-%d").to_string();
        let written =
            std::fs::read_to_string(tmp.path().join("Journal").join(format!("{}.md", today)))
                .unwrap();
        assert_eq!(
            written,
            "* [time:: 08:00] existing\n* [time:: 10:00] new entry\n"
        );
    }

    // --- template handling: local-file branch of fetch_template_content ---

    #[tokio::test]
    async fn ensure_day_file_applies_local_template_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".sb")).unwrap();
        std::fs::write(
            tmp.path().join(".sb").join("config.toml"),
            r#"[daily]
path = "Journal/{{date}}"
dateFormat = "%Y-%m-%d"
template = "Daily"
"#,
        )
        .unwrap();
        // Local template file
        std::fs::write(
            tmp.path().join("Daily.md"),
            "# Daily template\n\n## Notes\n",
        )
        .unwrap();

        let config = ResolvedConfig::load_from(tmp.path()).unwrap();
        let page_path = tmp.path().join("Journal").join("2026-05-18.md");
        let created = ensure_day_file(&page_path, tmp.path(), &config, None)
            .await
            .unwrap();
        assert!(created);
        let content = std::fs::read_to_string(&page_path).unwrap();
        assert_eq!(content, "# Daily template\n\n## Notes\n");
    }

    #[tokio::test]
    async fn fetch_template_content_local_file_wins() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("MyTemplate.md"), "local body").unwrap();
        let config = minimal_config(tmp.path());
        let got = super::fetch_template_content(tmp.path(), "MyTemplate", &config, None)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("local body"));
    }

    #[tokio::test]
    async fn fetch_template_content_returns_none_with_no_server_and_no_local() {
        let tmp = tempfile::tempdir().unwrap();
        let config = minimal_config(tmp.path());
        let got = super::fetch_template_content(tmp.path(), "Missing", &config, None)
            .await
            .unwrap();
        assert!(got.is_none());
    }
}
