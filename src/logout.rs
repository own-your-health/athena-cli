//! `auth logout` for one signed-in patient: revoke its tokens and delete its
//! sign-in, and do the same for a v0.2.x item at `<org>/<patient>`. Since
//! v0.3.0 nothing else reaches that item.

use std::fmt;

use pk_cli_auth::refresh_lock::RefreshLock;
use pk_cli_core::CliError;
use pk_cli_secrets::{CredentialStore, Secret};
use pk_med_smart::{SessionStore, SmartSession};
use serde::Deserialize;

use crate::http::Http;
use crate::oauth;
use crate::session;

/// The two keychain operations a v0.2.x item needs, as a seam for tests.
pub trait LegacyItems {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError>;
    fn delete(&self, account: &str) -> Result<bool, CliError>;
}

impl LegacyItems for CredentialStore {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        CredentialStore::get(self, account)
    }
    fn delete(&self, account: &str) -> Result<bool, CliError> {
        CredentialStore::delete(self, account)
    }
}

/// The keychain account v0.2.x used.
pub fn legacy_account(org: &str, patient: &str) -> String {
    format!("{org}/{patient}")
}

/// What a v0.2.x item (its own token JSON) holds for a revocation. `Debug`
/// shows no token.
#[derive(Deserialize)]
pub struct LegacySignIn {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub client_id: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

impl fmt::Debug for LegacySignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacySignIn")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// How one patient's sign-out went.
#[derive(Debug)]
pub struct SignedOut {
    /// A stored item was deleted, and no v0.2.x item was left behind.
    pub credential_removed: bool,
    /// Every revocation tried for the patient's items succeeded, and at
    /// least one was tried.
    pub revoked: bool,
    /// The v0.2.x item could not be read or deleted (a denied Keychain
    /// prompt): the patient stays in the index so `auth logout` can retry.
    pub left_behind: Option<CliError>,
}

/// Revoke and delete the sign-in at `smart/<org>/<patient>` in `store`, under
/// `lock` so a renewal in another process cannot store it again, then the
/// v0.2.x item in `legacy`. An item that does not read as a v0.2.x sign-in is
/// deleted without a revocation. The error names neither an account nor a
/// value.
pub fn sign_out(
    http: &dyn Http,
    lock: &RefreshLock,
    store: &dyn SessionStore,
    legacy: &dyn LegacyItems,
    org: &str,
    patient: &str,
) -> Result<SignedOut, CliError> {
    let (mut found, revoked) = revoke_and_delete(http, lock, store, &session::key(org, patient))?;
    let mut revocations: Vec<bool> = revoked.into_iter().collect();
    let account = legacy_account(org, patient);
    let left_behind = (|| {
        let Some(item) = legacy.get(&account)? else {
            return Ok(());
        };
        if let Ok(old) = serde_json::from_str::<LegacySignIn>(item.expose()) {
            revocations.push(revoke_legacy(http, &old));
        }
        found |= legacy.delete(&account)?;
        Ok(())
    })()
    .err()
    .map(|e: CliError| {
        CliError::Keychain(format!(
            "the v0.2.x sign-in at {org} could not be removed ({})",
            e.code()
        ))
    });
    Ok(SignedOut {
        credential_removed: found && left_behind.is_none(),
        revoked: !revocations.is_empty() && revocations.iter().all(|ok| *ok),
        left_behind,
    })
}

/// Revoke and delete the session at `account`, as [`sign_out`] does, for a
/// vault entry that no index row names: the account is used as the vault
/// lists it, never taken apart, and no v0.2.x item is looked for (those are
/// found through the index).
pub fn sign_out_account(
    http: &dyn Http,
    lock: &RefreshLock,
    store: &dyn SessionStore,
    account: &str,
) -> Result<SignedOut, CliError> {
    let (found, revoked) = revoke_and_delete(http, lock, store, account)?;
    Ok(SignedOut {
        credential_removed: found,
        revoked: revoked.unwrap_or(false),
        left_behind: None,
    })
}

/// Revoke the session at `account` if it reads, then delete it under
/// `lock`: whether there was one, and how its revocation went (`None` when
/// none was tried).
fn revoke_and_delete(
    http: &dyn Http,
    lock: &RefreshLock,
    store: &dyn SessionStore,
    account: &str,
) -> Result<(bool, Option<bool>), CliError> {
    let revoked = match store.load(account) {
        Ok(Some(stored)) => Some(revoke_all(http, &stored)),
        _ => None,
    };
    let found = lock.forget(|| store.delete(account))?;
    Ok((found, revoked))
}

/// Revoke the refresh and access tokens, best effort.
pub(crate) fn revoke_all(http: &dyn Http, stored: &SmartSession) -> bool {
    let Some(endpoint) = session::revocation_endpoint(stored) else {
        return false;
    };
    let tokens = &stored.tokens;
    revoke_each(
        http,
        endpoint,
        &stored.client_id,
        tokens.refresh_token.as_deref(),
        &tokens.access_token,
        "",
    )
}

/// Revoke what `dropped` holds of its own once `kept` replaces it, best
/// effort. Two copies of one grant (the same refresh token, a renewal having
/// changed only the access token) revoke nothing, since a server may end a
/// whole grant when either of its tokens is revoked; the dropped access token
/// lapses within the hour. Otherwise each token `kept` does not also hold is
/// revoked. `None` when there is nothing to revoke.
pub(crate) fn revoke_replaced(
    http: &dyn Http,
    kept: &SmartSession,
    dropped: &SmartSession,
) -> Option<bool> {
    let (kept, dropped_tokens) = (&kept.tokens, &dropped.tokens);
    if dropped_tokens.refresh_token.is_some() && dropped_tokens.refresh_token == kept.refresh_token
    {
        return None;
    }
    let refresh = dropped_tokens
        .refresh_token
        .as_deref()
        .filter(|t| kept.refresh_token.as_deref() != Some(*t));
    let access = Some(dropped_tokens.access_token.as_str()).filter(|t| *t != kept.access_token);
    if refresh.is_none() && access.is_none() {
        return None;
    }
    let Some(endpoint) = session::revocation_endpoint(dropped) else {
        return Some(false);
    };
    Some(revoke_tokens(
        http,
        endpoint,
        &dropped.client_id,
        refresh,
        access,
        "",
    ))
}

/// [`revoke_all`] for a v0.2.x sign-in.
fn revoke_legacy(http: &dyn Http, old: &LegacySignIn) -> bool {
    let Some(endpoint) = &old.revocation_endpoint else {
        return false;
    };
    revoke_each(
        http,
        endpoint,
        &old.client_id,
        old.refresh_token.as_deref(),
        &old.access_token,
        "v0.2.x ",
    )
}

fn revoke_each(
    http: &dyn Http,
    endpoint: &str,
    client_id: &str,
    refresh: Option<&str>,
    access: &str,
    which: &str,
) -> bool {
    revoke_tokens(http, endpoint, client_id, refresh, Some(access), which)
}

fn revoke_tokens(
    http: &dyn Http,
    endpoint: &str,
    client_id: &str,
    refresh: Option<&str>,
    access: Option<&str>,
    which: &str,
) -> bool {
    let mut ok = true;
    let tokens = refresh
        .map(|t| (t, "refresh_token"))
        .into_iter()
        .chain(access.map(|t| (t, "access_token")));
    for (token, hint) in tokens {
        if let Err(e) = oauth::revoke(http, endpoint, client_id, token, hint) {
            let name = hint.replace('_', " ");
            eprintln!(
                "warning: revoking the {which}{name} failed: {}",
                e.message()
            );
            ok = false;
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use pk_cli_core::transport::{HttpRequest, HttpResponse};
    use std::sync::Mutex;

    /// The token of each revocation sent, answered 200.
    #[derive(Default)]
    struct Sent(Mutex<Vec<String>>);

    impl Http for Sent {
        fn call(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
            let body = request.body.clone().unwrap_or_default();
            let token = body
                .split('&')
                .find_map(|pair| pair.strip_prefix("token="))
                .unwrap_or_default()
                .to_string();
            self.0.lock().unwrap().push(token);
            Ok(HttpResponse::new(200, ""))
        }
    }

    fn signed_in(refresh: Option<&str>, access: &str) -> SmartSession {
        let mut s = session::test_session(
            "clinic",
            "a-1.E-1",
            Some("https://idp.example.test/revoke"),
            0,
        );
        s.tokens.refresh_token = refresh.map(str::to_string);
        s.tokens.access_token = access.to_string();
        s
    }

    #[test]
    fn a_vault_entry_the_index_does_not_name_is_revoked_and_deleted_as_listed() {
        use pk_cli_auth::vault::{MemoryKeys, Vault};
        use pk_cli_config::profile::ProfileName;
        use pk_med_smart::ProfileSessions;
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::new(MemoryKeys::new(), dir.path().join("vault"));
        let sessions = ProfileSessions::new(&vault, ProfileName::parse("adult1").unwrap());
        // A patient id with a slash, which the account escapes.
        let mut stored = signed_in(Some("refresh-1"), "access-1");
        stored.patient = Some("a-1/E-1".into());
        SessionStore::save(&sessions, &stored).unwrap();
        let lock = RefreshLock::at(dir.path().join("refresh.lock"));
        let sent = Sent::default();
        let accounts = sessions.accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        let result = sign_out_account(&sent, &lock, &sessions, &accounts[0]).unwrap();
        assert!(result.credential_removed && result.revoked);
        assert_eq!(*sent.0.lock().unwrap(), ["refresh-1", "access-1"]);
        assert!(sessions.accounts().unwrap().is_empty());
        // Nothing there: nothing removed, nothing revoked.
        let again = sign_out_account(&sent, &lock, &sessions, &accounts[0]).unwrap();
        assert!(!again.credential_removed && !again.revoked);
    }

    #[test]
    fn a_replaced_sign_in_revokes_only_what_it_holds_alone() {
        // Two copies of one grant: nothing is sent, so the kept copy lives.
        let sent = Sent::default();
        let kept = signed_in(Some("refresh-1"), "access-2");
        let dropped = signed_in(Some("refresh-1"), "access-1");
        assert_eq!(revoke_replaced(&sent, &kept, &dropped), None);
        assert!(sent.0.lock().unwrap().is_empty());
        // The same tokens: nothing to revoke.
        assert_eq!(revoke_replaced(&sent, &kept, &kept), None);
        // Another grant: both of its tokens.
        let other = signed_in(Some("refresh-9"), "access-9");
        assert_eq!(revoke_replaced(&sent, &kept, &other), Some(true));
        assert_eq!(*sent.0.lock().unwrap(), ["refresh-9", "access-9"]);
        // No refresh tokens: the access token only.
        let sent = Sent::default();
        let plain = signed_in(None, "access-8");
        assert_eq!(
            revoke_replaced(&sent, &signed_in(None, "access-7"), &plain),
            Some(true)
        );
        assert_eq!(*sent.0.lock().unwrap(), ["access-8"]);
    }

    #[test]
    fn a_legacy_sign_in_reads_for_its_revocation_and_hides_its_tokens() {
        let item: LegacySignIn = serde_json::from_value(serde_json::json!({
            "access_token": "access-secret",
            "refresh_token": "refresh-secret",
            "expires_at": 1,
            "scope": "openid",
            "patient": "a-1.E-1",
            "client_id": "client",
            "fhir_base": "https://example.invalid/fhir/r4",
            "token_endpoint": "https://example.invalid/token",
            "revocation_endpoint": "https://example.invalid/revoke",
            "refreshed_at": 1
        }))
        .unwrap();
        assert_eq!(
            item.revocation_endpoint.as_deref(),
            Some("https://example.invalid/revoke")
        );
        let shown = format!("{item:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert_eq!(legacy_account("clinic", "a-1.E-1"), "clinic/a-1.E-1");
    }
}
