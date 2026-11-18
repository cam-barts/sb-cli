//! `sb mcp serve` — run sb as a Model Context Protocol server over the same core
//! as the CLI (one source of truth, two interfaces). Defaults to stdio (local
//! subprocess, zero network) or Streamable HTTP behind `--http`.
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
pub async fn execute_serve(
    _http: bool,
    _addr: Option<String>,
    _quiet: bool,
    _color: bool,
) -> crate::error::SbResult<()> {
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
        transport::{
            io::stdio,
            streamable_http_server::{
                session::local::LocalSessionManager, StreamableHttpServerConfig,
                StreamableHttpService,
            },
        },
        ErrorData, ServerHandler, ServiceExt,
    };
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

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
        ///
        /// `index.tag "NAME"` is the only source. A bare attribute name is an
        /// existence filter (`where zoteroKey`); `select a, b` projects fields
        /// and returns distinct rows, so one query can build a whole lookup
        /// table; comparison is `==`, never `=`.
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
        crate::runtime::eval(&client, "/.runtime/lua_script", &lua_script).await
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
        ensure_day_file(&page_path, &content_dir, &config, None, &date).await?;

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
        append_entry(&page_path, &entry_md, config.daily_entry_spacing.value)?;
        Ok(format!("appended to {}", page_name))
    }

    /// Default bind address for the Streamable HTTP transport.
    const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8787";

    /// Run the MCP server over stdio (default) or Streamable HTTP (`--http`).
    pub async fn execute_serve(
        http: bool,
        addr: Option<String>,
        quiet: bool,
        _color: bool,
    ) -> SbResult<()> {
        if http {
            return serve_http(addr, quiet).await;
        }
        // stdio: diagnostics go to stderr via tracing — stdout carries only
        // JSON-RPC, so nothing here may write to stdout.
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

    /// Serve over Streamable HTTP at `<addr>/mcp`, mounting rmcp's tower service
    /// on an axum router. A fresh `SbMcpServer` is created per session; sessions
    /// are held in memory (`LocalSessionManager`). Binds loopback by default;
    /// rmcp's default config restricts the `Host` header to localhost.
    async fn serve_http(addr: Option<String>, quiet: bool) -> SbResult<()> {
        let bind = addr.unwrap_or_else(|| DEFAULT_HTTP_ADDR.to_string());

        let service = StreamableHttpService::new(
            || Ok(SbMcpServer::new()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let app = axum::Router::new().nest_service("/mcp", service);

        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|e| SbError::Config {
                message: format!("failed to bind {bind}: {e}"),
            })?;

        // HTTP mode: stdout is not a protocol channel, but keep diagnostics on
        // stderr for consistency. Announce the endpoint unless suppressed.
        if !quiet {
            eprintln!("sb MCP server listening on http://{bind}/mcp");
        }
        tracing::info!("sb MCP server listening on http://{bind}/mcp");

        axum::serve(listener, app)
            .await
            .map_err(|e| SbError::Config {
                message: format!("MCP HTTP server error: {e}"),
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

        // --- tool behaviour against a real tempdir space / mock server ---
        //
        // The tool methods are plain async fns; calling them is exactly what
        // the MCP dispatcher does after decoding params, so these exercise the
        // observable result (files on disk, requests sent, error text).

        use crate::test_util::{make_space, SbSpaceGuard};
        use wiremock::matchers::{body_string_contains, method, path};
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

        fn create_args(name: &str, content: &str) -> Parameters<PageCreateArgs> {
            Parameters(PageCreateArgs {
                name: name.into(),
                content: content.into(),
            })
        }

        fn read_args(name: &str) -> Parameters<PageReadArgs> {
            Parameters(PageReadArgs { name: name.into() })
        }

        #[tokio::test]
        async fn page_create_writes_the_page_and_refuses_to_overwrite() {
            let tmp = make_space(None);
            let _g = SbSpaceGuard::set(tmp.path());
            let server = SbMcpServer::new();

            let msg = server
                .page_create(create_args("Projects/idea", "# Idea\n"))
                .await
                .unwrap();
            assert_eq!(msg, "created page 'Projects/idea'");
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("Projects/idea.md")).unwrap(),
                "# Idea\n"
            );

            let err = server
                .page_create(create_args("Projects/idea", "clobber"))
                .await
                .unwrap_err();
            assert!(err.message.contains("already exists"), "got: {err:?}");
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("Projects/idea.md")).unwrap(),
                "# Idea\n",
                "an existing page must never be overwritten"
            );
        }

        #[tokio::test]
        async fn page_create_rejects_path_traversal_and_writes_nothing() {
            let tmp = content_subdir_space();
            // Just outside the content dir, but still inside this test's own
            // tempdir: a leaked write is detectable and never touches /tmp.
            let outer = tmp.path().join("escape.md");
            let _g = SbSpaceGuard::set(tmp.path());

            let err = SbMcpServer::new()
                .page_create(create_args("../escape", "x"))
                .await
                .unwrap_err();
            assert!(err.message.contains("traversal"), "got: {err:?}");
            assert!(
                !outer.exists(),
                "traversal must not write outside the content dir"
            );
        }

        #[tokio::test]
        async fn page_read_returns_content_and_maps_missing_and_traversal_to_errors() {
            let tmp = content_subdir_space();
            std::fs::write(tmp.path().join("content/Note.md"), "hello\n").unwrap();
            let _g = SbSpaceGuard::set(tmp.path());
            let server = SbMcpServer::new();

            assert_eq!(
                server.page_read(read_args("Note")).await.unwrap(),
                "hello\n"
            );

            let missing = server.page_read(read_args("Nope")).await.unwrap_err();
            assert_eq!(missing.code, ErrorData::resource_not_found("", None).code);
            assert!(
                missing.message.contains("'Nope' not found"),
                "got: {missing:?}"
            );

            // A real file just outside the content dir must stay unreadable.
            std::fs::write(tmp.path().join("secret.md"), "secret").unwrap();
            let err = server.page_read(read_args("../secret")).await.unwrap_err();
            assert!(err.message.contains("traversal"), "got: {err:?}");
        }

        /// A space whose content lives in `content/`, so "outside the content
        /// dir" is still inside the test's own tempdir.
        fn content_subdir_space() -> tempfile::TempDir {
            let tmp = make_space(None);
            std::fs::write(
                tmp.path().join(".sb/config.toml"),
                "[sync]\ndir = \"content\"\n",
            )
            .unwrap();
            std::fs::create_dir_all(tmp.path().join("content")).unwrap();
            tmp
        }

        #[tokio::test]
        async fn page_list_returns_sorted_names_without_extension() {
            let tmp = make_space(None);
            std::fs::create_dir_all(tmp.path().join("Sub")).unwrap();
            std::fs::write(tmp.path().join("b.md"), "").unwrap();
            std::fs::write(tmp.path().join("a.md"), "").unwrap();
            std::fs::write(tmp.path().join("Sub/c.md"), "").unwrap();
            std::fs::write(tmp.path().join("image.png"), "").unwrap();
            let _g = SbSpaceGuard::set(tmp.path());

            let Json(list) = SbMcpServer::new().page_list().await.unwrap();
            assert_eq!(list.pages, vec!["Sub/c", "a", "b"]);
        }

        #[tokio::test]
        async fn query_runs_through_the_runtime_api_and_returns_the_result() {
            let mock = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/.runtime/lua_script"))
                .and(body_string_contains(
                    "return query[[from index.tag \"page\" limit 1]]",
                ))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(r#"{"result":[{"name":"Index"}]}"#),
                )
                .mount(&mock)
                .await;
            let tmp = make_space(Some(&mock.uri()));
            enable_runtime(tmp.path());
            let _g = SbSpaceGuard::set(tmp.path());

            let Json(res) = SbMcpServer::new()
                .query(Parameters(QueryArgs {
                    query: "from index.tag \"page\" limit 1".into(),
                }))
                .await
                .unwrap();
            assert_eq!(res.result, serde_json::json!([{"name": "Index"}]));
        }

        #[tokio::test]
        async fn query_without_runtime_api_errors_and_sends_no_request() {
            let mock = MockServer::start().await;
            let tmp = make_space(Some(&mock.uri()));
            let _g = SbSpaceGuard::set(tmp.path());

            let err = SbMcpServer::new()
                .query(Parameters(QueryArgs {
                    query: "from index.tag \"page\"".into(),
                }))
                .await
                .err()
                .expect("query must fail without the Runtime API");
            assert!(err.message.contains("Runtime API"), "got: {err:?}");
            assert!(mock.received_requests().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn server_ping_reports_reachable_and_surfaces_server_errors() {
            let mock = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.ping"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&mock)
                .await;
            // A down runtime (503 on the probe) must surface as runtime_api=false.
            Mock::given(method("POST"))
                .and(path("/.runtime/lua"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&mock)
                .await;
            let tmp = make_space(Some(&mock.uri()));
            let _g = SbSpaceGuard::set(tmp.path());

            let Json(ping) = SbMcpServer::new().server_ping().await.unwrap();
            assert!(ping.reachable);
            assert!(!ping.runtime_api, "a 503 probe means no Runtime API");

            mock.reset().await;
            Mock::given(method("GET"))
                .and(path("/.ping"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&mock)
                .await;
            let err = SbMcpServer::new()
                .server_ping()
                .await
                .err()
                .expect("a 401 must fail the ping");
            assert!(
                err.message.contains("authentication failed"),
                "got: {err:?}"
            );
        }

        #[tokio::test]
        async fn daily_append_adds_an_entry_to_todays_note() {
            let tmp = make_space(None);
            let _g = SbSpaceGuard::set(tmp.path());

            let msg = SbMcpServer::new()
                .daily_append(Parameters(DailyAppendArgs {
                    text: "met the team".into(),
                }))
                .await
                .unwrap();
            assert!(msg.starts_with("appended to Journal/"), "got: {msg}");

            let page = msg.strip_prefix("appended to ").unwrap();
            let body = std::fs::read_to_string(tmp.path().join(format!("{page}.md"))).unwrap();
            assert!(
                body.lines()
                    .any(|l| l.starts_with("* ") && l.ends_with("met the team")),
                "got: {body:?}"
            );
        }

        // Kept despite the audit: dropping `enable_tools()` from get_info
        // would make clients see a server with no tools, and no other test
        // reaches get_info.
        #[test]
        fn get_info_advertises_tools_capability() {
            let info = SbMcpServer::new().get_info();
            assert!(info.capabilities.tools.is_some());
            // Clients display this name; nothing else in the suite checks it.
            assert_eq!(info.server_info.name, "sb");
        }
    }
}
