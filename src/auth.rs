//! One saved personal access token per OS user, independent of projects
//! and worktrees. An explicit FIGMA_TOKEN always wins; an empty value
//! deliberately disables saved credentials (offline use and test isolation).

use std::ffi::OsString;

use keyring::Entry;
use serde_json::{Value, json};

const SERVICE: &str = "figmog";
const ACCOUNT: &str = "figma-token";
pub(crate) const MAX_TOKEN_BYTES: usize = 4096;

struct Credential {
    value: String,
    source: &'static str,
}

fn entry() -> Result<Entry, String> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(
            "saved credentials are supported on macOS and Linux; set FIGMA_TOKEN on this platform"
                .into(),
        );
    }
    Entry::new(SERVICE, ACCOUNT).map_err(|_| storage_error("open"))
}

// Do not format backend errors: BadEncoding, for example, can contain
// secret bytes. Errors and JSON results must never reveal the token.
fn storage_error(action: &str) -> String {
    format!(
        "could not {action} the saved Figma token in the system credential store — unlock macOS Keychain or start a Linux Secret Service; alternatively set FIGMA_TOKEN (an empty value disables saved credentials)"
    )
}

fn read_saved(entry: &Entry) -> Result<Option<String>, String> {
    match entry.get_password() {
        Ok(token) => normalize_token(&token).map(|token| Some(token.to_string())),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(storage_error("read")),
    }
}

fn resolve_with(
    environment: Option<OsString>,
    saved: impl FnOnce() -> Result<Option<String>, String>,
) -> Result<Option<Credential>, String> {
    if let Some(value) = environment {
        let value = value
            .into_string()
            .map_err(|_| "FIGMA_TOKEN must be valid UTF-8".to_string())?;
        if value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(Credential {
            value: normalize_token(&value)?.to_string(),
            source: "environment",
        }));
    }
    Ok(saved()?.map(|value| Credential {
        value,
        source: "credential_store",
    }))
}

fn resolve() -> Result<Option<Credential>, String> {
    resolve_with(std::env::var_os("FIGMA_TOKEN"), || read_saved(&entry()?))
}

/// Shared by serve, network pulls, and direct image downloads.
pub(crate) fn token() -> Result<Option<String>, String> {
    // A locked/unavailable store must not prevent an idle MCP server or
    // cached images from working offline. Network callers report missing
    // credentials; `auth status` keeps the storage error visible.
    let credential = resolve_with(std::env::var_os("FIGMA_TOKEN"), || {
        Ok(entry().and_then(|entry| read_saved(&entry)).unwrap_or(None))
    })?;
    Ok(credential.map(|credential| credential.value))
}

pub(crate) fn missing_token(operation: &str) -> String {
    format!(
        "FIGMA_TOKEN not set and no saved token is available — required for {operation}; run `figmog auth login` or set FIGMA_TOKEN (`figmog auth status` checks credential-store access)"
    )
}

/// Validate only the input shape, without a network request or requiring
/// extra Figma scopes. Expiration and file permissions are checked by Figma
/// when the token is actually used.
pub(crate) fn normalize_token(token: &str) -> Result<&str, String> {
    let token = token.trim();
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
        return Err("token must contain between 1 and 4096 bytes".into());
    }
    if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("token must be a single line of ASCII text without spaces".into());
    }
    Ok(token)
}

fn save_to(entry: &Entry, token: &str) -> Result<(), String> {
    let token = normalize_token(token)?;
    entry.set_password(token).map_err(|_| storage_error("save"))
}

pub(crate) fn login(token: &str) -> Result<Value, String> {
    // Reject invalid input before accessing the system store.
    let token = normalize_token(token)?;
    save_to(&entry()?, token)?;
    Ok(json!({
        "saved": true,
        "storage": "credential_store",
        "environment_override": std::env::var_os("FIGMA_TOKEN").is_some(),
    }))
}

pub(crate) fn status() -> Result<Value, String> {
    let credential = resolve()?;
    Ok(json!({
        "configured": credential.is_some(),
        "source": credential.map(|credential| credential.source),
        "validated": false,
    }))
}

fn remove_from(entry: &Entry) -> Result<bool, String> {
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(_) => Err(storage_error("remove")),
    }
}

pub(crate) fn logout() -> Result<Value, String> {
    let removed = remove_from(&entry()?)?;
    Ok(json!({
        "removed": removed,
        "environment_override": std::env::var_os("FIGMA_TOKEN").is_some(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring::mock::MockCredential;

    fn mock_entry() -> Entry {
        Entry::new_with_credential(Box::new(MockCredential::default()))
    }

    #[test]
    fn saved_token_can_be_replaced_resolved_and_removed() {
        let entry = mock_entry();
        assert!(read_saved(&entry).unwrap().is_none());
        save_to(&entry, "figd_first").unwrap();
        save_to(&entry, "figd_replacement\n").unwrap();
        let credential = resolve_with(None, || read_saved(&entry)).unwrap().unwrap();
        assert_eq!(credential.value, "figd_replacement");
        assert_eq!(credential.source, "credential_store");
        assert!(remove_from(&entry).unwrap());
        assert!(!remove_from(&entry).unwrap());
        assert!(resolve_with(None, || read_saved(&entry)).unwrap().is_none());
    }

    #[test]
    fn environment_overrides_saved_credentials_without_accessing_the_store() {
        let credential = resolve_with(Some("figd_environment".into()), || {
            panic!("must not access the credential store with an environment override")
        })
        .unwrap()
        .unwrap();
        assert_eq!(credential.value, "figd_environment");
        assert_eq!(credential.source, "environment");
        assert!(
            resolve_with(Some("".into()), || panic!("offline override"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_input_is_rejected_without_replacing_a_saved_token() {
        let entry = mock_entry();
        save_to(&entry, "figd_original").unwrap();
        for invalid in ["", " \n", "figd_a\nfigd_b", "figd_\0secret", "figd_é"] {
            assert!(save_to(&entry, invalid).is_err());
        }
        assert!(save_to(&entry, &"x".repeat(MAX_TOKEN_BYTES + 1)).is_err());
        assert_eq!(read_saved(&entry).unwrap().unwrap(), "figd_original");
    }

    #[test]
    fn credential_store_failures_are_actionable_and_never_expose_backend_secrets() {
        let entry = mock_entry();
        let mock = entry
            .get_credential()
            .downcast_ref::<MockCredential>()
            .unwrap();
        mock.set_error(keyring::Error::PlatformFailure(Box::new(
            std::io::Error::other("figd_backend_secret"),
        )));
        let error = read_saved(&entry).unwrap_err();
        assert!(error.contains("credential store"));
        assert!(error.contains("FIGMA_TOKEN"));
        assert!(!error.contains("figd_backend_secret"));

        mock.set_error(keyring::Error::BadEncoding(b"figd_raw_secret".to_vec()));
        let error = read_saved(&entry).unwrap_err();
        assert!(!error.contains("figd_raw_secret"));

        mock.set_error(keyring::Error::NoStorageAccess(Box::new(
            std::io::Error::other("locked"),
        )));
        assert!(save_to(&entry, "figd_new").unwrap_err().contains("save"));
        mock.set_error(keyring::Error::NoStorageAccess(Box::new(
            std::io::Error::other("locked"),
        )));
        assert!(remove_from(&entry).unwrap_err().contains("remove"));
    }

    #[test]
    fn invalid_environment_never_falls_back_to_saved_credentials() {
        assert!(
            resolve_with(Some("figd_a\nfigd_b".into()), || panic!("invalid override")).is_err()
        );
        use std::os::unix::ffi::OsStringExt;
        assert!(
            resolve_with(Some(OsString::from_vec(vec![0xff])), || {
                panic!("invalid encoding")
            })
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires an unlocked native credential store; uses a disposable synthetic entry"]
    fn native_credential_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let service = format!(
            "figmog-test-{}",
            dir.path().file_name().unwrap().to_string_lossy()
        );
        struct Cleanup(Entry);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.delete_credential();
            }
        }
        let entry = Cleanup(Entry::new(&service, ACCOUNT).unwrap());
        save_to(&entry.0, "figd_synthetic_test_only").unwrap();
        assert_eq!(
            read_saved(&entry.0).unwrap().unwrap(),
            "figd_synthetic_test_only"
        );
        assert!(remove_from(&entry.0).unwrap());
        assert!(read_saved(&entry.0).unwrap().is_none());
    }
}
