use crate::error::{SbError, SbResult};
use tracing::debug;

/// Service name used as the keychain service identifier.
/// The server URL is used as the "username" field to support multiple servers.
const SERVICE_NAME: &str = "sb-cli";

/// Read an auth token from the OS keychain for the given server URL.
///
/// Returns `Ok(Some(token))` if found, `Ok(None)` if no entry exists.
/// Returns `Err` on keychain access errors (locked keychain, permission denied).
pub fn get_token(server_url: &str) -> SbResult<Option<String>> {
    let entry = keyring::Entry::new(SERVICE_NAME, server_url).map_err(|e| SbError::Config {
        message: format!("keychain error: failed to create entry: {e}"),
    })?;

    match entry.get_password() {
        Ok(password) => {
            debug!("token found in OS keychain for {}", server_url);
            Ok(Some(password))
        }
        Err(keyring::Error::NoEntry) => {
            debug!("no keychain entry found for {}", server_url);
            Ok(None)
        }
        Err(e) => {
            debug!("keychain access error: {e}");
            Err(SbError::Config {
                message: format!("keychain error: {e}"),
            })
        }
    }
}

/// Store an auth token in the OS keychain for the given server URL.
pub fn set_token(server_url: &str, token: &str) -> SbResult<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, server_url).map_err(|e| SbError::Config {
        message: format!("keychain error: failed to create entry: {e}"),
    })?;

    entry.set_password(token).map_err(|e| SbError::Config {
        message: format!("keychain error: failed to store token: {e}"),
    })?;

    debug!("token stored in OS keychain for {}", server_url);
    Ok(())
}

/// Delete an auth token from the OS keychain for the given server URL.
/// Returns Ok(()) even if no entry existed.
pub fn delete_token(server_url: &str) -> SbResult<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, server_url).map_err(|e| SbError::Config {
        message: format!("keychain error: failed to create entry: {e}"),
    })?;

    match entry.delete_credential() {
        Ok(()) => {
            debug!("token deleted from OS keychain for {}", server_url);
            Ok(())
        }
        Err(keyring::Error::NoEntry) => Ok(()), // Already absent -- not an error
        Err(e) => Err(SbError::Config {
            message: format!("keychain error: failed to delete token: {e}"),
        }),
    }
}
