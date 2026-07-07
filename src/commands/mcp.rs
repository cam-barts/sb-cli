//! `sb mcp serve` — run sb as a Model Context Protocol server over the same core
//! as the CLI (one source of truth, two interfaces). Defaults to stdio (local
//! subprocess, zero network); Streamable HTTP is not yet implemented.
//!
//! CRITICAL: in stdio mode nothing may be written to stdout except JSON-RPC —
//! all diagnostics must go to stderr, or the protocol stream is corrupted. This
//! module therefore never uses `println!`/`print!`; diagnostics go through
//! `tracing`, which the binary routes to stderr.
//!
//! The tool catalog is deliberately small and outcome-oriented rather than a
//! 1:1 mirror of the CLI. Each tool reuses the same lower-level helpers the CLI
//! commands call, so there is a single source of truth for space resolution,
//! token handling, and server access.

// When the `mcp` feature is disabled the module still compiles (it is declared
// unconditionally in `commands/mod.rs`), but `execute_serve` is a thin stub.
#[cfg(not(feature = "mcp"))]
pub async fn execute_serve(_http: bool, _quiet: bool, _color: bool) -> crate::error::SbResult<()> {
    Err(crate::error::SbError::Usage(
        "this `sb` binary was built without the `mcp` feature".into(),
    ))
}

#[cfg(feature = "mcp")]
pub use imp::execute_serve;

#[cfg(feature = "mcp")]
mod imp {
    use rmcp::{
        handler::server::{
            router::tool::ToolRouter,
            wrapper::{Json, Parameters},
        },
        model::{Implementation, ServerCapabilities, ServerInfo},
        tool, tool_handler, tool_router,
        transport::io::stdio,
        ErrorData, ServerHandler, ServiceExt,
    };
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use crate::commands::page::{
        find_content_dir, find_space_root, resolve_page_path, validate_page_path,
    };
    use crate::commands::server::{build_client, runtime_unavailable_error};
    use crate::config::ResolvedConfig;
    use crate::error::{SbError, SbResult};

    /// Convert an internal `SbError` into an MCP tool error. Tool call failures
    /// are surfaced to the model as JSON-RPC errors carrying the message.
    fn to_mcp_err(e: SbError) -> ErrorData {
        ErrorData::internal_error(e.to_string(), None)
    }

    // --- Tool argument structs (deserialized from JSON-RPC params) ---

    /// Arguments for the `page_read` tool.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct PageReadArgs {
        /// Page name without the `.md` extension, e.g. `Journal/2026-07-07`.
        name: String,
    }

    /// Arguments for the `query` tool.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct QueryArgs {
        /// A SilverBullet index query (SLIQ) body, e.g.
        /// `from index.tag "page" order by name limit 10`.
        query: String,
    }

    /// Arguments for the `daily_append` tool.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct DailyAppendArgs {
        /// Text to append as a new journal entry to today's daily note.
        text: String,
    }

    /// Arguments for the `page_create` tool.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct PageCreateArgs {
        /// Page name without the `.md` extension, e.g. `Projects/new-idea`.
        name: String,
        /// Markdown content for the new page.
        content: String,
    }

    // --- Structured tool outputs ---
    //
    // The MCP spec requires a tool's structured `outputSchema` to have an
    // `object` root, so raw arrays / scalars are wrapped in named objects.

    /// Structured result for the `page_list` tool.
    #[derive(Debug, Serialize, JsonSchema)]
    struct PageListResult {
        /// Page names (without `.md`), sorted.
        pages: Vec<String>,
    }

    /// Structured result for the `query` tool.
    #[derive(Debug, Serialize, JsonSchema)]
    struct QueryResult {
        /// The raw JSON value returned by the query.
        result: serde_json::Value,
    }

    /// Structured result for the `server_ping` tool.
    #[derive(Debug, Serialize, JsonSchema)]
    struct PingResult {
        /// Whether the server responded to the ping.
        reachable: bool,
        /// Round-trip time in milliseconds.
        response_ms: u64,
        /// Whether the SilverBullet Runtime API is available.
        runtime_api: bool,
    }

    /// The MCP server handler. Stateless — every tool resolves the space, config,
    /// and token the same way the CLI does, so it always reflects current config.
    #[derive(Clone)]
    pub struct SbMcpServer {
        tool_router: ToolRouter<Self>,
    }

    impl SbMcpServer {
        pub fn new() -> Self {
            Self {
                tool_router: Self::tool_router(),
            }
        }
    }

    #[tool_router]
    impl SbMcpServer {
        /// List the names of every page in the local space.
        #[tool(
            description = "List all page names in the local SilverBullet space.",
            annotations(title = "List pages", read_only_hint = true)
        )]
        async fn page_list(&self) -> Result<Json<PageListResult>, ErrorData> {
            let content_dir = find_content_dir().map_err(to_mcp_err)?;
            let names = crate::commands::page::list_page_names(&content_dir).map_err(to_mcp_err)?;
            Ok(Json(PageListResult { pages: names }))
        }

        /// Read a page's markdown by name from the local space.
        #[tool(
            description = "Read a page's markdown content by name from the local space.",
            annotations(title = "Read page", read_only_hint = true)
        )]
        async fn page_read(
            &self,
            Parameters(args): Parameters<PageReadArgs>,
        ) -> Result<String, ErrorData> {
            let content_dir = find_content_dir().map_err(to_mcp_err)?;
            // Path-traversal guard runs before any filesystem access.
            validate_page_path(&content_dir, &args.name).map_err(to_mcp_err)?;
            let page_path = resolve_page_path(&content_dir, &args.name);
            if !page_path.exists() {
                return Err(ErrorData::resource_not_found(
                    format!("page '{}' not found", args.name),
                    None,
                ));
            }
            std::fs::read_to_string(&page_path)
                .map_err(|e| ErrorData::internal_error(format!("failed to read page: {e}"), None))
        }

        /// Run a SilverBullet index query (SLIQ), returning the result as JSON.
        #[tool(
            description = "Run a SilverBullet index query (SLIQ) against the server, returning JSON.",
            annotations(title = "Run query", read_only_hint = true)
        )]
        async fn query(
            &self,
            Parameters(args): Parameters<QueryArgs>,
        ) -> Result<Json<QueryResult>, ErrorData> {
            let result = run_query(&args.query).await.map_err(to_mcp_err)?;
            Ok(Json(QueryResult { result }))
        }

        /// Check SilverBullet server connectivity.
        #[tool(
            description = "Check SilverBullet server connectivity and report response time and Runtime API availability.",
            annotations(title = "Ping server", read_only_hint = true)
        )]
        async fn server_ping(&self) -> Result<Json<PingResult>, ErrorData> {
            let client = build_client(None).map_err(to_mcp_err)?;
            let elapsed = client.ping().await.map_err(to_mcp_err)?;
            let runtime_api = match find_space_root() {
                Ok(space_root) => {
                    crate::runtime::detect_runtime_api(&client, &space_root.join(".sb")).await
                }
                Err(_) => false,
            };
            Ok(Json(PingResult {
                reachable: true,
                response_ms: elapsed.as_millis() as u64,
                runtime_api,
            }))
        }

        /// Append a line to today's daily journal note (creating it if needed).
        #[tool(
            description = "Append a line to today's daily journal note, creating the note from the configured template if it does not yet exist.",
            annotations(
                title = "Append to daily note",
                read_only_hint = false,
                destructive_hint = false,
                idempotent_hint = false
            )
        )]
        async fn daily_append(
            &self,
            Parameters(args): Parameters<DailyAppendArgs>,
        ) -> Result<String, ErrorData> {
            append_to_today(&args.text).await.map_err(to_mcp_err)
        }

        /// Create a new page with the given markdown content.
        #[tool(
            description = "Create a new page with the given markdown content. Fails if the page already exists (never overwrites).",
            annotations(
                title = "Create page",
                read_only_hint = false,
                destructive_hint = false,
                idempotent_hint = false
            )
        )]
        async fn page_create(
            &self,
            Parameters(args): Parameters<PageCreateArgs>,
        ) -> Result<String, ErrorData> {
            let content_dir = find_content_dir().map_err(to_mcp_err)?;
            let page_path = validate_page_path(&content_dir, &args.name).map_err(to_mcp_err)?;
            if page_path.exists() {
                return Err(ErrorData::invalid_params(
                    format!("page '{}' already exists", args.name),
                    None,
                ));
            }
            if let Some(parent) = page_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ErrorData::internal_error(
                        format!("failed to create parent directories: {e}"),
                        None,
                    )
                })?;
            }
            std::fs::write(&page_path, &args.content).map_err(|e| {
                ErrorData::internal_error(format!("failed to write page: {e}"), None)
            })?;
            Ok(format!("created page '{}'", args.name))
        }
    }

    #[tool_handler(router = self.tool_router)]
    impl ServerHandler for SbMcpServer {
        fn get_info(&self) -> ServerInfo {
            // ServerInfo is #[non_exhaustive]; build from default then set fields.
            let mut info = ServerInfo::default();
            info.capabilities = ServerCapabilities::builder().enable_tools().build();
            info.server_info = Implementation::new("sb", env!("CARGO_PKG_VERSION"));
            info.instructions = Some(
                "sb exposes a local SilverBullet space. Read tools (page_list, \
                 page_read, query, server_ping) never modify the space; \
                 daily_append and page_create make additive changes only."
                    .to_string(),
            );
            info
        }
    }

    /// Run a SLIQ query against the server via the Runtime API, mirroring
    /// `commands::query::execute` but returning the parsed JSON rather than
    /// printing it.
    async fn run_query(query: &str) -> SbResult<serde_json::Value> {
        let space_root = find_space_root()?;
        let config = ResolvedConfig::load_from(&space_root)?;
        if !config.runtime_available.value {
            return Err(runtime_unavailable_error());
        }
        let client = build_client(None)?;
        let lua_script = format!("return query[[{}]]", query);
        let resp = client
            .post_text("/.runtime/lua_script", &lua_script)
            .await?;
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
                body: format!("Query error: {error}"),
            });
        }
        Ok(parsed
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Append `text` as a formatted journal entry to today's daily note,
    /// reusing the same daily-note helpers the `sb daily` command uses.
    async fn append_to_today(text: &str) -> SbResult<String> {
        use crate::commands::daily::{
            append_entry, ensure_day_file, format_daily_path, format_entry, resolve_daily_date,
        };

        let space_root = find_space_root()?;
        let config = ResolvedConfig::load_from(&space_root)?;
        let content_dir = space_root.join(&config.sync_dir.value);

        let date = resolve_daily_date(0)?;
        let page_name = format_daily_path(
            &config.daily_path.value,
            &config.daily_date_format.value,
            &date,
        );
        let page_path = validate_page_path(&content_dir, &page_name)?;
        ensure_day_file(&page_path, &content_dir, &config, None).await?;

        let time = jiff::Zoned::now()
            .strftime(&config.daily_time_format.value)
            .to_string();
        let bullet = config
            .daily_bullet_style
            .value
            .chars()
            .next()
            .unwrap_or('*');
        let entry_md = format_entry(Some(&time), false, bullet, text, false, None);
        append_entry(&page_path, &entry_md)?;
        Ok(format!("appended to {}", page_name))
    }

    /// Run the MCP server. stdio is the supported transport; `--http` is not yet
    /// implemented.
    pub async fn execute_serve(http: bool, _quiet: bool, _color: bool) -> SbResult<()> {
        if http {
            return Err(SbError::Usage(
                "HTTP transport not yet supported; use stdio".into(),
            ));
        }
        // Diagnostics go to stderr via tracing — stdout carries only JSON-RPC.
        tracing::info!("starting sb MCP server on stdio");
        let running = SbMcpServer::new()
            .serve(stdio())
            .await
            .map_err(|e| SbError::Config {
                message: format!("failed to start MCP server: {e}"),
            })?;
        running.waiting().await.map_err(|e| SbError::Config {
            message: format!("MCP server terminated abnormally: {e}"),
        })?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Compile-level smoke test: the router must register exactly the tools we
        // advertise, with the read tools flagged read-only. No I/O, no server.
        #[test]
        fn router_registers_expected_tools() {
            let server = SbMcpServer::new();
            let tools = server.tool_router.list_all();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
            for expected in [
                "page_list",
                "page_read",
                "query",
                "server_ping",
                "daily_append",
                "page_create",
            ] {
                assert!(names.contains(&expected), "missing tool: {expected}");
            }

            let read_only: std::collections::HashMap<&str, bool> = tools
                .iter()
                .map(|t| {
                    (
                        t.name.as_ref(),
                        t.annotations
                            .as_ref()
                            .and_then(|a| a.read_only_hint)
                            .unwrap_or(false),
                    )
                })
                .collect();
            assert_eq!(read_only.get("page_list"), Some(&true));
            assert_eq!(read_only.get("query"), Some(&true));
            assert_eq!(read_only.get("daily_append"), Some(&false));
        }

        #[test]
        fn get_info_advertises_tools_capability() {
            let info = SbMcpServer::new().get_info();
            assert!(info.capabilities.tools.is_some());
            assert_eq!(info.server_info.name, "sb");
        }
    }
}
