use clap::{Parser, Subcommand};

/// Stable exit-code contract, shown under `sb --help`. Agents branch on these
/// codes (and the matching `code` field of `--format json` errors) without
/// parsing stderr. Never reshuffled.
const EXIT_CODE_HELP: &str = "\
Exit codes:
  0  success
  1  general error
  2  usage / invalid arguments
  3  authentication error
  4  not found
  5  conflict / already exists
  6  confirmation required (re-run with --yes)
  *  `sb shell` passes through the remote process's own exit code

With `--format json`, failures also print `{\"error\",\"code\",\"remediation\"}` to stderr.";

/// Worked `sb query` examples, shown under `sb query --help`. Every line here
/// was verified against a real space; the notes are the four things that
/// otherwise cost an afternoon of trial and error (bare-attribute filters,
/// `select` projecting *and* de-duplicating, single-field results coming back
/// flat, and `=` not being a comparison).
const QUERY_EXAMPLES: &str = "\
Examples:
  # newest pages first
  sb query 'from index.tag \"page\" order by lastModified desc limit 10'

  # a bare attribute name is an existence filter: pages that have a zoteroKey
  sb query 'from index.tag \"page\" where zoteroKey'

  # select projects fields -- and returns DISTINCT rows
  sb query 'from index.tag \"page\" where zoteroKey select name, zoteroKey'

  # one query for a whole lookup table, not one query per key
  sb query 'from index.tag \"page\" where zoteroKey select zoteroKey, name' \\
    | jq 'map({(.zoteroKey): .name}) | add'

  # open tasks for one assignee
  sb query 'from index.tag \"task\" where done == false and assignee == \"cam\"'

  # tags are namespaced, and each level is its own source
  sb query 'from index.tag \"zotero/journalArticle\" limit 5'

Notes:
  `index.tag \"NAME\"` is the only query source. Run `sb describe` for the tag
  names that exist in this space, and `sb describe NAME` for the attributes
  those objects carry -- that is what you can filter and select on.

  Comparison is `==`, never `=`; a single `=` is a syntax error.
  Selecting exactly one field returns a flat array of values, not objects.
  `limit` caps rows; `order by FIELD [desc]` sorts them.";

#[derive(Parser)]
#[command(
    name = "sb",
    about = "CLI tool for interacting with SilverBullet",
    version,
    propagate_version = true,
    after_help = EXIT_CODE_HELP
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

    /// Never prompt: disable interactive pickers, confirmations, and $EDITOR
    /// launches (also implied when stdin/stdout is not a TTY). Agents should set
    /// this to guarantee sb never blocks on input it cannot provide.
    #[arg(long, global = true)]
    pub no_input: bool,

    /// Assume "yes" to confirmation prompts on destructive operations. Required
    /// (or `--force`) for agents to run mutations non-interactively.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Request timeout in seconds. Raises both the local HTTP timeout and the
    /// Runtime API's own `X-Timeout`, which otherwise each default to 30 and
    /// race each other on a slow query.
    #[arg(long, global = true, value_name = "SECONDS")]
    pub timeout: Option<u64>,
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
        /// Sign the entry with `-- @name`, crediting who wrote it. Repeatable.
        /// Credits an author rather than addressing a recipient, so an agent
        /// signing its own entry does not queue a mention for itself.
        #[arg(long, value_name = "NAME")]
        sign: Vec<String>,

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
    ///
    /// A bare expression goes to `/.runtime/lua`, which does NOT accept
    /// statements: `sb lua 'return 1+1'` is a Lua syntax error, not a server
    /// fault. Use `--script FILE`, or `--script -` to read stdin, for anything
    /// with statements or an explicit `return`.
    Lua {
        /// Lua expression to evaluate. Omit when using --script.
        expression: Option<String>,
        /// Run a multi-statement Lua script from a file. Use `-` for stdin.
        #[arg(long, value_name = "FILE", conflicts_with = "expression")]
        script: Option<String>,
    },
    /// Execute an index query via the Runtime API
    #[command(after_help = QUERY_EXAMPLES, after_long_help = QUERY_EXAMPLES)]
    Query {
        /// Query expression (e.g., `from index.tag "page" limit 10`)
        query: String,
        /// Restrict JSON output to these comma-separated top-level fields
        #[arg(long, value_delimiter = ',', value_name = "FIELD,...")]
        fields: Vec<String>,
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
        /// Maximum entries to request. The server retains up to 1000.
        #[arg(long, short = 'n', value_name = "N")]
        lines: Option<usize>,
    },
    /// Save a PNG screenshot of the SilverBullet headless browser
    Screenshot {
        /// Output path. Use `-` for stdout. Defaults to ./sb-screenshot-<utc>.png
        /// when stdout is a TTY, otherwise raw PNG bytes are written to stdout.
        #[arg(long, short = 'o')]
        output: Option<String>,
    },
    /// Describe the observed schema of objects tagged with the given name,
    /// or list every tag in the index when no tag is given
    Describe {
        /// Tag name to introspect (e.g. task, page, link). Omit to list all
        /// indexed tags with their object counts.
        tag: Option<String>,
        /// Number of objects to sample when inferring the schema
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Restrict JSON output to these comma-separated top-level fields
        /// (`tag`, `sampled`, `fields`; in list mode `name`, `count`, `parents`)
        #[arg(long = "fields", value_delimiter = ',', value_name = "FIELD,...")]
        out_fields: Vec<String>,
    },
    /// Show wiki links between pages, from the server's relation index
    Links {
        /// Page name to report on. Omit to pick interactively.
        page: Option<String>,
        /// Show links pointing AT this page (backlinks). The default.
        #[arg(long)]
        to: bool,
        /// Show links pointing OUT of this page instead of at it
        #[arg(long, conflicts_with = "to")]
        from: bool,
        /// Maximum number of relations to return
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Restrict JSON output to these comma-separated top-level fields
        #[arg(long, value_delimiter = ',', value_name = "FIELD,...")]
        fields: Vec<String>,
    },
    /// List open `@mention`s addressed to an identity (the Mention Inbox)
    Inbox {
        /// Identity to list mentions for, with or without the leading `@`.
        /// Falls back to the `identity` key in the space config.
        #[arg(long, value_name = "NAME")]
        to: Option<String>,
        /// Maximum number of mentions to return
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Restrict JSON output to these comma-separated top-level fields
        #[arg(long, value_delimiter = ',', value_name = "FIELD,...")]
        fields: Vec<String>,
    },
    /// Work with page templates (pages tagged `meta/template/page`)
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
    /// Update sb to the latest GitHub release (matching this build's flavor)
    Upgrade {
        /// Report whether a newer release is available without installing it
        #[arg(long)]
        check: bool,
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
    /// Serve as an MCP server over stdio (default) or Streamable HTTP (--http)
    Serve {
        /// Serve over Streamable HTTP instead of stdio (endpoint: /mcp)
        #[arg(long)]
        http: bool,
        /// Address to bind for --http (default: 127.0.0.1:8787)
        #[arg(long, value_name = "HOST:PORT", requires = "http")]
        addr: Option<String>,
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
    /// Resolve a sync conflict (omit the path to pick from the conflict list)
    Resolve {
        /// File path relative to space root (e.g., Journal/2026-04-05.md).
        /// Omit to pick interactively from the files currently in conflict.
        path: Option<String>,
        /// Resolve every conflicted file. Needs --keep-local, --keep-remote,
        /// --force or --diff when stdin is not a terminal, since there is
        /// nobody to answer the per-file prompt.
        #[arg(long, conflicts_with = "path")]
        all: bool,
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
    /// Delete conflict stashes under .sb/conflicts/ that carry no information:
    /// ones byte-identical to the live local file, and duplicates of a newer
    /// stash. A path still in conflict keeps its newest stash so `sb sync
    /// resolve` has something to diff against.
    PruneStashes {
        /// Only prune stashes for this path (relative to the space root).
        /// Omit to sweep every stashed path.
        path: Option<String>,
        /// Also remove every stash for paths that are no longer in conflict,
        /// and waive the keep-the-newest rule for ones that still are.
        #[arg(long)]
        all: bool,
        /// List what would be pruned without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub enum TemplateCommands {
    /// List pages tagged `meta/template/page`
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
        /// Preview the page that would be created without writing anything
        #[arg(long)]
        dry_run: bool,
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
        /// Restrict JSON output to these comma-separated top-level fields
        #[arg(long, value_delimiter = ',', value_name = "FIELD,...")]
        fields: Vec<String>,
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
        /// Overwrite the page if it already exists (idempotent create) instead
        /// of failing with a conflict
        #[arg(long)]
        upsert: bool,
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
        /// Preview the deletion without removing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Append content to a page
    Append {
        /// Page name (without .md extension)
        name: String,
        /// Content to append
        #[arg(long)]
        content: String,
        /// Sign the appended block with `-- @name`, crediting who wrote it.
        /// Repeatable. A signature credits an author; it does NOT address a
        /// recipient, so it never lands in anyone's Mention Inbox.
        #[arg(long, value_name = "NAME")]
        sign: Vec<String>,
    },
    /// List a page's revision history
    History {
        /// Page name (without .md extension). Omit to pick interactively.
        name: Option<String>,
        /// Maximum revisions to return (server caps this at 200)
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Page back from this revision hash
        #[arg(long, value_name = "HASH")]
        before: Option<String>,
    },
    /// Show what a revision changed in a page, as a unified diff
    Diff {
        /// Page name (without .md extension). Omit to pick interactively.
        name: Option<String>,
        /// Full 40-character commit hash. Omit to diff uncommitted changes
        /// (HEAD versus what is on disk).
        #[arg(long, value_name = "HASH")]
        rev: Option<String>,
    },
    /// Restore a page to an earlier revision
    ///
    /// Writes the old content to the LOCAL file and leaves it for the next
    /// sync to push, so the restore goes through the same conflict handling as
    /// any other local edit rather than around it.
    Restore {
        /// Page name (without .md extension). Omit to pick interactively.
        name: Option<String>,
        /// Full 40-character commit hash to restore
        #[arg(long, value_name = "HASH")]
        rev: String,
        /// Skip the confirmation prompt
        #[arg(long)]
        force: bool,
    },
    /// Move/rename a page
    Move {
        /// Current page name (without .md extension)
        name: String,
        /// New page name (without .md extension)
        new_name: String,
        /// Preview the move without changing anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum SortField {
    Name,
    Modified,
    Created,
}
