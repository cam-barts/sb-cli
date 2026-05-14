use crate::cli::OutputFormat;
use crate::client::{RuntimeLogEntry, RuntimeLogs};
use crate::commands::server::{build_client, require_initialized, runtime_unavailable_error};
use crate::error::{SbError, SbResult};
use console::Style;
use jiff::Timestamp;

/// Execute `sb logs` — fetch client + server logs from the SilverBullet
/// runtime and render them. Streams pretty output when stdout is a TTY and
/// the format is `Human`; otherwise emits newline-delimited JSON (one entry
/// per line) so the output is easy to grep / pipe.
pub async fn execute(
    cli_token: Option<&str>,
    follow: bool,
    interval_ms: u64,
    source: LogSource,
    format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    require_initialized()?;
    let client = build_client(cli_token)?;

    // Track the last timestamp we've already printed when --follow is on
    // so the polling loop only emits new entries. `None` means "first pass --
    // include every entry, including those without timestamps".
    let mut high_water: Option<i64> = None;
    let mut first = true;
    loop {
        let logs = match client.get_runtime_logs().await {
            Ok(l) => l,
            Err(SbError::HttpStatus { status: 503, .. }) => {
                return Err(runtime_unavailable_error());
            }
            Err(e) => return Err(e),
        };

        let entries = filter_and_sort(&logs, source, high_water);
        for entry in &entries {
            render_entry(entry, format, color);
            if let Some(ts) = entry.entry.timestamp {
                high_water = Some(high_water.map_or(ts, |w| w.max(ts)));
            }
        }

        if !follow {
            if first && entries.is_empty() && !quiet && matches!(format, OutputFormat::Human) {
                eprintln!("No log entries.");
            }
            return Ok(());
        }

        first = false;
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
}

/// Which side of the log stream to display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// Both client and server logs interleaved.
    Both,
    /// Client (browser) logs only.
    Client,
    /// Server (runtime/headless) logs only.
    Server,
}

/// Internal: an entry tagged with which stream it came from, ready to render.
#[derive(Debug, Clone)]
pub(crate) struct TaggedEntry<'a> {
    pub stream: &'static str,
    pub entry: &'a RuntimeLogEntry,
}

/// Filter and sort entries from the runtime payload.
///
/// - When `after == None` (initial pass), all entries are returned, including
///   those without timestamps.
/// - When `after == Some(ts)` (--follow tailing), only entries strictly newer
///   than `ts` are returned. Entries without timestamps cannot be deduplicated
///   in follow mode and are dropped.
/// - Output is sorted by timestamp ascending; entries without timestamps sort
///   first so a reader sees them before the time-ordered tail.
pub(crate) fn filter_and_sort(
    logs: &RuntimeLogs,
    source: LogSource,
    after: Option<i64>,
) -> Vec<TaggedEntry<'_>> {
    let keep = |e: &&RuntimeLogEntry| match (after, e.timestamp) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(t), Some(et)) => et > t,
    };
    let mut out: Vec<TaggedEntry<'_>> = Vec::new();
    if matches!(source, LogSource::Both | LogSource::Client) {
        out.extend(logs.client_logs.iter().filter(keep).map(|e| TaggedEntry {
            stream: "client",
            entry: e,
        }));
    }
    if matches!(source, LogSource::Both | LogSource::Server) {
        out.extend(logs.server_logs.iter().filter(keep).map(|e| TaggedEntry {
            stream: "server",
            entry: e,
        }));
    }
    out.sort_by_key(|t| t.entry.timestamp.unwrap_or(i64::MIN));
    out
}

fn render_entry(entry: &TaggedEntry<'_>, format: &OutputFormat, color: bool) {
    match format {
        OutputFormat::Json => {
            // NDJSON: one entry per line, including stream tag.
            let json = serde_json::json!({
                "stream": entry.stream,
                "level": entry.entry.level,
                "message": entry.entry.message,
                "timestamp": entry.entry.timestamp,
            });
            println!("{}", json);
        }
        OutputFormat::Human => {
            let ts = entry
                .entry
                .timestamp
                .and_then(|ms| Timestamp::from_millisecond(ms).ok())
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".into());
            let level = entry.entry.level.as_deref().unwrap_or("log");
            let stream = entry.stream;
            if color {
                let stream_style = match stream {
                    "client" => Style::new().cyan(),
                    "server" => Style::new().magenta(),
                    _ => Style::new(),
                };
                let level_style = match level {
                    "error" => Style::new().red().bold(),
                    "warn" | "warning" => Style::new().yellow().bold(),
                    _ => Style::new().dim(),
                };
                println!(
                    "{} {} {} {}",
                    Style::new().dim().apply_to(&ts),
                    stream_style.apply_to(format!("[{stream}]")),
                    level_style.apply_to(format!("{level:>5}")),
                    entry.entry.message,
                );
            } else {
                println!("{} [{}] {:>5} {}", ts, stream, level, entry.entry.message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(message: &str, level: Option<&str>, ts: Option<i64>) -> RuntimeLogEntry {
        RuntimeLogEntry {
            level: level.map(str::to_string),
            message: message.to_string(),
            timestamp: ts,
        }
    }

    #[test]
    fn filter_and_sort_orders_by_timestamp_ascending() {
        let logs = RuntimeLogs {
            client_logs: vec![entry("b", Some("log"), Some(20))],
            server_logs: vec![entry("a", Some("log"), Some(10))],
            extra: Default::default(),
        };
        let sorted = filter_and_sort(&logs, LogSource::Both, None);
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].entry.message, "a");
        assert_eq!(sorted[1].entry.message, "b");
    }

    #[test]
    fn filter_and_sort_drops_entries_at_or_before_after_cursor() {
        let logs = RuntimeLogs {
            client_logs: vec![
                entry("old", Some("log"), Some(5)),
                entry("new", Some("log"), Some(15)),
            ],
            server_logs: vec![],
            extra: Default::default(),
        };
        let sorted = filter_and_sort(&logs, LogSource::Both, Some(10));
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].entry.message, "new");
    }

    #[test]
    fn filter_and_sort_client_only_omits_server() {
        let logs = RuntimeLogs {
            client_logs: vec![entry("c", Some("log"), Some(1))],
            server_logs: vec![entry("s", Some("log"), Some(2))],
            extra: Default::default(),
        };
        let only_client = filter_and_sort(&logs, LogSource::Client, None);
        assert_eq!(only_client.len(), 1);
        assert_eq!(only_client[0].stream, "client");
    }

    #[test]
    fn filter_and_sort_server_only_omits_client() {
        let logs = RuntimeLogs {
            client_logs: vec![entry("c", Some("log"), Some(1))],
            server_logs: vec![entry("s", Some("log"), Some(2))],
            extra: Default::default(),
        };
        let only_server = filter_and_sort(&logs, LogSource::Server, None);
        assert_eq!(only_server.len(), 1);
        assert_eq!(only_server[0].stream, "server");
    }

    #[test]
    fn filter_and_sort_entries_without_timestamps_appear_first_on_initial_pass() {
        let logs = RuntimeLogs {
            client_logs: vec![
                entry("ts", Some("log"), Some(100)),
                entry("nots", Some("log"), None),
            ],
            server_logs: vec![],
            extra: Default::default(),
        };
        let sorted = filter_and_sort(&logs, LogSource::Both, None);
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].entry.message, "nots");
        assert_eq!(sorted[1].entry.message, "ts");
    }

    #[test]
    fn filter_and_sort_drops_timestampless_entries_in_follow_mode() {
        let logs = RuntimeLogs {
            client_logs: vec![
                entry("ts", Some("log"), Some(100)),
                entry("nots", Some("log"), None),
            ],
            server_logs: vec![],
            extra: Default::default(),
        };
        let sorted = filter_and_sort(&logs, LogSource::Both, Some(50));
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].entry.message, "ts");
    }
}
