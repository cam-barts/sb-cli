use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "sb",
    about = "CLI tool for interacting with SilverBullet",
    version,
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Suppress all informational output
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Enable detailed logging to stderr
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Output format: `human` for tables/colors or `json` for machine-readable.
    /// When unset, defaults to `human` if stdout is a TTY and `json` otherwise.
    #[arg(long, global = true)]
    pub format: Option<OutputFormat>,

    /// Auth token override (highest precedence)
    #[arg(long, global = true)]
    pub token: Option<String>,
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum OutputFormat {
    #[value(alias = "table")]
    Human,
    Json,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show version information
    Version,
    /// Show resolved configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Initialize a local space linked to a SilverBullet server
    Init {
        /// Server URL (e.g., https://sb.example.com)
        server_url: String,
    },
    /// Server management commands
    Server {
        #[command(subcommand)]
        command: ServerCommands,
    },
    /// Manage authentication
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Manage pages in the local space
    Page {
        #[command(subcommand)]
        command: PageCommands,
    },
    /// Open, write, or list daily journal entries
    ///
    /// Positional text is appended as a timestamped bullet to today's note.
    /// When stdin is piped and no positional text is given, stdin becomes the entry.
    /// With no text and no view flag, opens the day's note in $EDITOR.
    /// View flags (-n, --from, --to, --on, --contains, --tags, --starred, --short)
    /// switch to read mode and list past entries; positional text in read mode
    /// is treated as a --contains filter.
    ///
    /// Entry text may begin with a date prefix to route to another day:
    /// "today: ...", "yesterday: ...", or "YYYY-MM-DD: ...".
    Daily {
        /// Entry text. Joined with spaces. In read mode, treated as a --contains filter.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        entry: Vec<String>,

        /// Target yesterday's note (write or read)
        #[arg(long)]
        yesterday: bool,
        /// Target the note N days from today (negative = past)
        #[arg(long, allow_hyphen_values = true, conflicts_with = "on")]
        offset: Option<i64>,
        /// Target a specific date (YYYY-MM-DD). Read-mode: filter to this day only.
        #[arg(long, value_name = "YYYY-MM-DD", conflicts_with = "yesterday")]
        on: Option<String>,

        /// Star this entry ([starred:: true] attribute)
        #[arg(long)]
        star: bool,
        /// Override the entry time (HH:MM)
        #[arg(long, value_name = "HH:MM")]
        time: Option<String>,
        /// Omit the time attribute on this entry
        #[arg(long, conflicts_with = "time")]
        no_time: bool,
        /// Write the entry as a task (checkbox item: `* [ ] ...`)
        #[arg(long)]
        task: bool,
        /// Tag applied to the task (implies --task; overrides the configured default)
        #[arg(long, value_name = "TAG", conflicts_with = "no_task_tag")]
        task_tag: Option<String>,
        /// Suppress the task tag for this entry (implies --task)
        #[arg(long)]
        no_task_tag: bool,
        /// Legacy synonym for the positional entry (kept for back-compat)
        #[arg(long, value_name = "TEXT")]
        append: Option<String>,

        /// List the most recent N matching entries (triggers read mode)
        #[arg(long, short = 'n', value_name = "N")]
        limit: Option<usize>,
        /// List entries from this date onward (YYYY-MM-DD; triggers read mode)
        #[arg(long, value_name = "YYYY-MM-DD")]
        from: Option<String>,
        /// List entries up to this date (YYYY-MM-DD; triggers read mode)
        #[arg(long, value_name = "YYYY-MM-DD")]
        to: Option<String>,
        /// Filter entries containing this substring (case-insensitive; triggers read mode)
        #[arg(long, value_name = "TEXT")]
        contains: Option<String>,
        /// Filter entries with at least one of these #tags (comma-separated; triggers read mode)
        #[arg(long, value_delimiter = ',', value_name = "TAG")]
        tags: Vec<String>,
        /// Show only starred entries (triggers read mode)
        #[arg(long)]
        starred: bool,
        /// One-line-per-entry rendering in read mode
        #[arg(long)]
        short: bool,
    },
    /// Sync local space with the server
    Sync {
        #[command(subcommand)]
        command: Option<SyncCommands>,
        /// Preview actions without executing
        #[arg(long)]
        dry_run: bool,
        /// Number of concurrent upload/download workers (overrides config and SB_SYNC_WORKERS)
        #[arg(long)]
        workers: Option<u32>,
    },
    /// Evaluate a Space Lua expression via the Runtime API
    Lua {
        /// Lua expression to evaluate
        expression: String,
    },
    /// Execute an index query via the Runtime API
    Query {
        /// Query expression (e.g., "from tags.page limit 10")
        query: String,
    },
    /// Execute a command on the server via the shell endpoint
    Shell {
        /// Command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Fetch buffered client and server logs from the SilverBullet runtime
    Logs {
        /// Continue polling and print new entries as they arrive
        #[arg(long, short = 'f')]
        follow: bool,
        /// Polling interval in milliseconds when --follow is set
        #[arg(long, default_value_t = 2000)]
        interval_ms: u64,
        /// Which side to show: both (default), client, or server
        #[arg(long, value_enum, default_value = "both")]
        source: LogSourceArg,
    },
    /// Save a PNG screenshot of the SilverBullet headless browser
    Screenshot {
        /// Output path. Use `-` for stdout. Defaults to ./sb-screenshot-<utc>.png
        /// when stdout is a TTY, otherwise raw PNG bytes are written to stdout.
        #[arg(long, short = 'o')]
        output: Option<String>,
    },
    /// Describe the observed schema of objects tagged with the given name
    Describe {
        /// Tag name to introspect (e.g. task, page, template)
        tag: String,
        /// Number of objects to sample when inferring the schema
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Work with page templates (pages tagged `template`)
    Template {
        #[command(subcommand)]
        command: TemplateCommands,
    },
    /// Generate or install shell completion scripts
    Completions {
        /// Shell to generate for. Auto-detected from $SHELL when omitted with --install.
        shell: Option<clap_complete::Shell>,
        /// Install to the standard location for the shell instead of printing to stdout
        #[arg(long)]
        install: bool,
    },
    /// Emit the full command surface as machine-readable JSON (source of truth for agents)
    #[cfg(feature = "skills")]
    Schema,
    /// Generate agent instruction files (AGENTS.md, SKILL.md, ...) describing how to drive sb
    #[cfg(feature = "skills")]
    Skills {
        #[command(subcommand)]
        command: SkillsCommands,
    },
    /// Run sb as a Model Context Protocol (MCP) server
    #[cfg(feature = "mcp")]
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },
}

/// Subcommands for `sb skills`.
#[cfg(feature = "skills")]
#[derive(Subcommand)]
pub enum SkillsCommands {
    /// Write agent instruction/skill files into the current directory
    Init {
        /// Which ecosystem file(s) to generate.
        #[arg(long, value_enum, default_value_t = SkillsTarget::Agents)]
        target: SkillsTarget,
    },
}

/// Target ecosystem for `sb skills init`.
#[cfg(feature = "skills")]
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SkillsTarget {
    /// AGENTS.md — the cross-tool baseline (default)
    Agents,
    /// CLAUDE.md + .claude/skills/<name>/SKILL.md
    Claude,
    /// .cursor/rules/*.mdc
    Cursor,
    /// .github/copilot-instructions.md
    Copilot,
    /// .windsurf/rules/*.md (Devin)
    Windsurf,
    /// Every supported target
    All,
}

/// Subcommands for `sb mcp`.
#[cfg(feature = "mcp")]
#[derive(Subcommand)]
pub enum McpCommands {
    /// Serve over stdio (default) or Streamable HTTP (--http)
    Serve {
        /// Serve over Streamable HTTP instead of stdio (binds a local port)
        #[arg(long)]
        http: bool,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum LogSourceArg {
    Both,
    Client,
    Server,
}

impl From<LogSourceArg> for crate::commands::logs::LogSource {
    fn from(value: LogSourceArg) -> Self {
        match value {
            LogSourceArg::Both => Self::Both,
            LogSourceArg::Client => Self::Client,
            LogSourceArg::Server => Self::Server,
        }
    }
}

#[derive(Subcommand)]
pub enum SyncCommands {
    /// Pull changes from the server
    Pull {
        /// Preview actions without executing
        #[arg(long)]
        dry_run: bool,
        /// Number of concurrent download workers (overrides config and SB_SYNC_WORKERS)
        #[arg(long)]
        workers: Option<u32>,
    },
    /// Push local changes to the server
    Push {
        /// Preview actions without executing
        #[arg(long)]
        dry_run: bool,
        /// Number of concurrent upload workers (overrides config and SB_SYNC_WORKERS)
        #[arg(long)]
        workers: Option<u32>,
    },
    /// Show sync status (modified, new, deleted, conflicts)
    Status,
    /// List files in conflict
    Conflicts,
    /// Resolve a sync conflict
    Resolve {
        /// File path relative to space root (e.g., Journal/2026-04-05.md)
        path: String,
        /// Keep the local version (upload to server)
        #[arg(long, conflicts_with = "keep_remote")]
        keep_local: bool,
        /// Keep the remote version (overwrite local)
        #[arg(long, conflicts_with = "keep_local")]
        keep_remote: bool,
        /// Show diff between local and stashed remote
        #[arg(long)]
        diff: bool,
        /// Apply default resolution (keep local) without prompting
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum TemplateCommands {
    /// List pages tagged as templates
    List,
    /// Create a new page from a template (interactive picker when --template is omitted)
    New {
        /// Page name to create (without .md extension). When omitted, the
        /// template's `suggestedName` is used (confirmed interactively unless the
        /// template sets `confirmName: false`).
        name: Option<String>,
        /// Template page to use (skips the picker)
        #[arg(long)]
        template: Option<String>,
        /// Do not open the new page in $EDITOR after creation (opens by default
        /// when running in a terminal)
        #[arg(long)]
        no_edit: bool,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Display resolved configuration with source annotations
    Show {
        /// Reveal masked values (auth tokens)
        #[arg(long)]
        reveal: bool,
    },
    /// Set the default space path in XDG config (~/.config/sb/config.toml)
    SetSpace {
        /// Space path (absolute or ~/relative)
        path: String,
    },
    /// Show the currently resolved space root and its source
    GetSpace,
}

#[derive(Subcommand)]
pub enum ServerCommands {
    /// Check server connectivity and response time
    Ping,
    /// Display server configuration
    Config,
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// Set the auth token for the current space
    Set {
        /// Token value (if omitted, prompts interactively)
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PageCommands {
    /// List all pages in the local space
    List {
        /// Sort field for listing pages
        #[arg(long, default_value = "name", value_enum)]
        sort: SortField,
        /// Limit number of results
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Read a page's content
    Read {
        /// Page name (without .md extension). Omit to pick interactively.
        name: Option<String>,
        /// Fetch from server instead of local
        #[arg(long)]
        remote: bool,
    },
    /// Create a new page
    Create {
        /// Page name (without .md extension)
        name: String,
        /// Page content (alternative to editor)
        #[arg(long)]
        content: Option<String>,
        /// Open in editor after creation
        #[arg(long)]
        edit: bool,
        /// Use template page as initial content
        #[arg(long)]
        template: Option<String>,
    },
    /// Edit a page in $EDITOR
    Edit {
        /// Page name (without .md extension). Omit to pick interactively.
        name: Option<String>,
    },
    /// Delete a page
    Delete {
        /// Page name (without .md extension). Omit to pick interactively.
        name: Option<String>,
        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,
    },
    /// Append content to a page
    Append {
        /// Page name (without .md extension)
        name: String,
        /// Content to append
        #[arg(long)]
        content: String,
    },
    /// Move/rename a page
    Move {
        /// Current page name (without .md extension)
        name: String,
        /// New page name (without .md extension)
        new_name: String,
    },
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum SortField {
    Name,
    Modified,
    Created,
}
