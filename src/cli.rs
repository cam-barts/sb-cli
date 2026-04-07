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

    /// Output format: human or json
    #[arg(long, global = true, default_value = "human")]
    pub format: OutputFormat,

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
    /// Open today's daily note
    Daily {
        /// Append text without opening editor
        #[arg(long)]
        append: Option<String>,
        /// Target yesterday's note
        #[arg(long)]
        yesterday: bool,
        /// Target note N days from today (negative = past)
        #[arg(long, allow_hyphen_values = true)]
        offset: Option<i64>,
    },
    /// Sync local space with the server
    Sync {
        #[command(subcommand)]
        command: Option<SyncCommands>,
        /// Preview actions without executing
        #[arg(long)]
        dry_run: bool,
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
}

#[derive(Subcommand)]
pub enum SyncCommands {
    /// Pull changes from the server
    Pull {
        /// Preview actions without executing
        #[arg(long)]
        dry_run: bool,
    },
    /// Push local changes to the server
    Push {
        /// Preview actions without executing
        #[arg(long)]
        dry_run: bool,
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
pub enum ConfigCommands {
    /// Display resolved configuration with source annotations
    Show {
        /// Reveal masked values (auth tokens)
        #[arg(long)]
        reveal: bool,
    },
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
        /// Page name (without .md extension)
        name: String,
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
        /// Page name (without .md extension)
        name: String,
    },
    /// Delete a page
    Delete {
        /// Page name (without .md extension)
        name: String,
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
