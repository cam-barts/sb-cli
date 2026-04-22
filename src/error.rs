pub type SbResult<T> = std::result::Result<T, SbError>;

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SbError {
    /// Network-level failure -- cannot reach server
    #[error("could not connect to {url}")]
    Network {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// HTTP non-success status -- not 401/403 which are AuthFailed
    #[error("server returned {status} for {url}")]
    HttpStatus {
        status: u16,
        url: String,
        body: String,
    },

    /// Authentication failure (401/403) -- to be used in Phase 2
    #[error("authentication failed for {url}")]
    AuthFailed { url: String, status: u16 },

    /// Configuration error
    #[error("{message}")]
    Config { message: String },

    /// File system error
    #[error("{message}: {path}")]
    Filesystem {
        message: String,
        path: String,
        #[source]
        source: Option<std::io::Error>,
    },

    /// Database error
    #[error("{message}")]
    Database {
        message: String,
        #[source]
        source: Option<rusqlite::Error>,
    },

    /// Internal error -- unexpected state or logic bug
    #[error("internal error: {message}")]
    Internal { message: String },

    /// Usage error -- bad arguments, invalid flags
    #[error("{0}")]
    Usage(String),

    /// Not yet implemented -- placeholder for future commands
    #[error("command not yet implemented")]
    NotImplemented,

    /// Space is already initialized (`.sb/` directory already exists)
    #[error("already initialized: {path}")]
    AlreadyInitialized { path: String },

    /// Command requires initialization but no `.sb/` directory was found
    #[error("not initialized: no .sb/ directory found")]
    NotInitialized,

    /// Space path is configured (via env or XDG config) but no `.sb/` exists there
    #[error("space not found at {configured_path} (configured via {via})")]
    SpaceNotFound { configured_path: String, via: String },

    /// No auth token found from any source
    #[error("no auth token found")]
    TokenNotFound { checked: Vec<String> },

    /// Page not found in the local space or on the server
    #[error("page not found: {name}")]
    PageNotFound { name: String },

    /// Attempted to create a page that already exists
    #[error("page already exists: {name}")]
    PageAlreadyExists { name: String },

    /// No $EDITOR environment variable configured
    #[error("no editor configured")]
    EditorNotSet,

    /// Remote process exited with non-zero code (used by sb shell)
    #[error("process exited with code {code}")]
    ProcessFailed { code: i32, stderr: String },
}

impl SbError {
    /// Exit code: 0 success, 1 general error, 2 usage error
    pub fn exit_code(&self) -> i32 {
        match self {
            SbError::Usage(_) => 2,
            SbError::ProcessFailed { code, .. } => *code,
            _ => 1,
        }
    }

    /// Hint text for actionable errors.
    /// Returns None when the error is self-explanatory.
    pub fn hint(&self) -> Option<String> {
        match self {
            SbError::Network { .. } => {
                Some("Check that the server URL is correct and the server is running".to_string())
            }
            SbError::AuthFailed { .. } => {
                Some("Check your token with: sb config show --reveal".to_string())
            }
            SbError::HttpStatus { status, .. } if *status == 404 => {
                Some("The requested resource was not found on the server".to_string())
            }
            SbError::NotInitialized => Some(
                "Run `sb init <server-url>` to initialize, or set `space = \"...\"` in ~/.config/sb/config.toml".to_string()
            ),
            SbError::SpaceNotFound { configured_path, via } => Some(format!(
                "Check that {configured_path} exists and contains a .sb/ directory, or update the path in {via}"
            )),
            SbError::TokenNotFound { checked } => Some(format!("Checked: {}", checked.join(", "))),
            SbError::PageNotFound { .. } => {
                Some("Run `sb page list` to see available pages".to_string())
            }
            SbError::PageAlreadyExists { name } => Some(format!(
                "Page '{}' already exists; use `sb page edit` to modify it",
                name
            )),
            SbError::EditorNotSet => {
                Some("Set the $EDITOR environment variable (e.g., export EDITOR=vim)".to_string())
            }
            SbError::Database { .. } => {
                Some("check that .sb/state.db is accessible and not corrupted".to_string())
            }
            SbError::ProcessFailed { stderr, .. } if !stderr.is_empty() => {
                Some(format!("stderr: {stderr}"))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_error_has_exit_code_1() {
        let err = SbError::Network {
            url: "http://localhost:3000".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            )),
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn usage_error_has_exit_code_2() {
        let err = SbError::Usage("bad argument".to_string());
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn http_status_error_has_exit_code_1() {
        let err = SbError::HttpStatus {
            status: 500,
            url: "http://localhost:3000/.fs/test".to_string(),
            body: "Internal Server Error".to_string(),
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn config_error_has_exit_code_1() {
        let err = SbError::Config {
            message: "invalid config".to_string(),
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn auth_failed_has_exit_code_1() {
        let err = SbError::AuthFailed {
            url: "http://localhost:3000".to_string(),
            status: 401,
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn not_implemented_has_exit_code_1() {
        let err = SbError::NotImplemented;
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn filesystem_error_has_exit_code_1() {
        let err = SbError::Filesystem {
            message: "could not read file".to_string(),
            path: "/tmp/test.md".to_string(),
            source: None,
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn network_error_has_hint_about_server_url() {
        let err = SbError::Network {
            url: "http://localhost:3000".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            )),
        };
        let hint = err.hint().expect("network error should have a hint");
        assert!(hint.contains("server URL"));
    }

    #[test]
    fn auth_failed_has_hint_about_token() {
        let err = SbError::AuthFailed {
            url: "http://localhost:3000".to_string(),
            status: 401,
        };
        let hint = err.hint().expect("auth error should have a hint");
        assert!(hint.contains("config show --reveal"));
    }

    #[test]
    fn http_404_has_hint_about_not_found() {
        let err = SbError::HttpStatus {
            status: 404,
            url: "http://localhost:3000/.fs/missing".to_string(),
            body: "Not Found".to_string(),
        };
        let hint = err.hint().expect("404 error should have a hint");
        assert!(hint.contains("not found"));
    }

    #[test]
    fn http_500_has_no_hint() {
        let err = SbError::HttpStatus {
            status: 500,
            url: "http://localhost:3000/.fs/test".to_string(),
            body: "Internal Server Error".to_string(),
        };
        assert!(err.hint().is_none());
    }

    #[test]
    fn config_error_has_no_hint() {
        let err = SbError::Config {
            message: "bad config".to_string(),
        };
        assert!(err.hint().is_none());
    }

    #[test]
    fn usage_error_has_no_hint() {
        let err = SbError::Usage("bad argument".to_string());
        assert!(err.hint().is_none());
    }

    #[test]
    fn http_status_error_message_contains_status_and_url() {
        let err = SbError::HttpStatus {
            status: 404,
            url: "http://localhost:3000/.fs/test".to_string(),
            body: "Not Found".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("404"), "message should contain status code");
        assert!(
            msg.contains("http://localhost:3000/.fs/test"),
            "message should contain URL"
        );
    }

    #[test]
    fn network_error_message_contains_url() {
        let err = SbError::Network {
            url: "http://localhost:3000".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            )),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("http://localhost:3000"),
            "message should contain URL"
        );
    }

    // --- New variant tests ---

    #[test]
    fn already_initialized_has_exit_code_1() {
        let err = SbError::AlreadyInitialized {
            path: "/home/user/notes".to_string(),
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn already_initialized_message_contains_path() {
        let err = SbError::AlreadyInitialized {
            path: "/home/user/notes".to_string(),
        };
        assert!(err.to_string().contains("/home/user/notes"));
    }

    #[test]
    fn already_initialized_has_no_hint() {
        let err = SbError::AlreadyInitialized {
            path: "/home/user/notes".to_string(),
        };
        assert!(err.hint().is_none());
    }

    #[test]
    fn not_initialized_has_exit_code_1() {
        let err = SbError::NotInitialized;
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn not_initialized_hint_mentions_sb_init() {
        let err = SbError::NotInitialized;
        let hint = err.hint().expect("NotInitialized should have a hint");
        assert!(hint.contains("sb init"));
    }

    #[test]
    fn token_not_found_has_exit_code_1() {
        let err = SbError::TokenNotFound {
            checked: vec!["--token flag".to_string()],
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn token_not_found_hint_contains_checked() {
        let err = SbError::TokenNotFound {
            checked: vec![
                "--token flag".to_string(),
                "SB_TOKEN environment variable".to_string(),
                ".sb/config.toml token field".to_string(),
            ],
        };
        let hint = err.hint().expect("TokenNotFound should have a hint");
        assert!(hint.contains("Checked:"));
        assert!(hint.contains("--token flag"));
        assert!(hint.contains("SB_TOKEN environment variable"));
    }

    #[test]
    fn token_not_found_message_is_descriptive() {
        let err = SbError::TokenNotFound {
            checked: vec!["--token flag".to_string()],
        };
        assert_eq!(err.to_string(), "no auth token found");
    }

    // --- Phase 03 new variant tests ---

    #[test]
    fn page_not_found_has_exit_code_1() {
        let err = SbError::PageNotFound {
            name: "test".into(),
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn page_already_exists_has_exit_code_1() {
        let err = SbError::PageAlreadyExists {
            name: "test".into(),
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn editor_not_set_has_exit_code_1() {
        let err = SbError::EditorNotSet;
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn page_not_found_hint_contains_sb_page_list() {
        let err = SbError::PageNotFound {
            name: "test".into(),
        };
        let hint = err.hint().expect("PageNotFound should have a hint");
        assert!(
            hint.contains("sb page list"),
            "hint should mention sb page list, got: {hint}"
        );
    }

    #[test]
    fn editor_not_set_hint_contains_editor_env_var() {
        let err = SbError::EditorNotSet;
        let hint = err.hint().expect("EditorNotSet should have a hint");
        assert!(
            hint.contains("$EDITOR"),
            "hint should mention $EDITOR, got: {hint}"
        );
    }

    #[test]
    fn page_already_exists_hint_contains_page_name() {
        let err = SbError::PageAlreadyExists {
            name: "my-page".into(),
        };
        let hint = err.hint().expect("PageAlreadyExists should have a hint");
        assert!(
            hint.contains("my-page"),
            "hint should contain page name, got: {hint}"
        );
    }
}
