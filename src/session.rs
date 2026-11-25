//! Signed-in patients: one `pk_med_smart::SmartSession` per (login profile,
//! practice profile, patient), in athena's vault (`pk_cli_auth::vault`) at
//! entry `<profile>/<org>/<patient>`, and a non-secret index of which exist.
//!
//! The vault keeps every sign-in in its own encrypted file under one keychain
//! item, its key, so a new or upgraded build asks macOS once, not once per
//! sign-in. The index (`sessions.v2.json` beside the config, owner-only;
//! v0.3.x's `sessions.json` is read until it exists, and never written) lets
//! `patients list` and `--patient <label>` work without reading the vault.
//!
//! Before v0.4.0 each sign-in was its own keychain item at
//! `smart/<org>/<patient>`, in no profile; `athena profiles migrate` moves it
//! into one.

use std::path::Path;
use std::sync::Arc;

use pk_cli_auth::token::DEFAULT_SKEW_SECS;
use pk_cli_auth::vault::Vault;
use pk_cli_config::profile::ProfileName;
use pk_cli_core::{CliError, Reason};
use pk_cli_secrets::CredentialStore;
use pk_med_smart::{ProfileSessions, SessionStore, SmartSession};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::env::Environment;
use crate::BIN;

/// The keychain service. `ATHENA_KEYCHAIN_SERVICE` overrides it for tests
/// only, so no test or `make verify` step reads the owner's real items; the
/// vault then lives beside the config instead of in `~/.config/athena/vault`.
pub const KEYCHAIN_SERVICE: &str = "piekstra.athena";
pub const KEYCHAIN_SERVICE_ENV: &str = "ATHENA_KEYCHAIN_SERVICE";

/// athenahealth: a refresh token expires 90 days after the most recent token
/// request that used or generated it.
pub const REFRESH_LIFETIME_SECS: u64 = 90 * 24 * 60 * 60;

/// Where a session keeps athenahealth's revocation endpoint, for `auth
/// logout` (`SmartSession::extra`, which renewals carry over).
pub const REVOCATION_ENDPOINT: &str = "revocation_endpoint";

/// The environment variable `--profile` falls back to.
pub const PROFILE_ENV: &str = "ATHENA_PROFILE";

/// `smart/<org>/<patient>`: the account of one sign-in within a profile, and
/// the keychain account of a sign-in stored before v0.4.0.
pub fn key(org: &str, patient: &str) -> String {
    pk_med_smart::account(org, patient)
}

/// Where sessions live, shareable with the FHIR client's bearer source.
pub type Store = Arc<dyn SessionStore + Send + Sync>;

/// The keychain service in use: `piekstra.athena`, or the test override.
pub fn service() -> String {
    service_override().unwrap_or_else(|| KEYCHAIN_SERVICE.to_string())
}

fn service_override() -> Option<String> {
    std::env::var(KEYCHAIN_SERVICE_ENV)
        .ok()
        .filter(|s| !s.is_empty())
}

/// The raw keychain store: the items sign-ins were before v0.4.0, which
/// `profiles migrate` moves, and `auth logout`'s v0.2.x items.
pub fn credential_store() -> CredentialStore {
    CredentialStore::new(service())
}

/// athena's vault: its key in the keychain service, its entries in
/// `~/.config/athena/vault/`, or under the test override beside the config
/// in `state_dir`, so a test never opens the owner's vault.
pub fn vault(state_dir: &Path) -> Result<Arc<Vault>, CliError> {
    let vault = match service_override() {
        Some(service) => Vault::new(CredentialStore::new(service), state_dir.join("vault")),
        None => Vault::for_binary(BIN)?,
    };
    Ok(Arc::new(vault))
}

/// One profile's sign-ins in the vault.
pub fn profile_store(vault: Arc<Vault>, profile: ProfileName) -> Store {
    Arc::new(ProfileStore { vault, profile })
}

/// `pk_med_smart::ProfileSessions` over a vault it shares, with a read that
/// fails for any reason but a refused or missing credential tagged
/// `CredentialUnreadable`, the code `auth status` has reported since v0.1.0.
struct ProfileStore {
    vault: Arc<Vault>,
    profile: ProfileName,
}

impl ProfileStore {
    fn sessions(&self) -> ProfileSessions<'_> {
        ProfileSessions::new(&self.vault, self.profile.clone())
    }
}

impl SessionStore for ProfileStore {
    fn load(&self, account: &str) -> Result<Option<SmartSession>, CliError> {
        self.sessions().load(account).map_err(|e| {
            if e.is_auth() {
                e
            } else {
                e.because(Reason::CredentialUnreadable)
            }
        })
    }
    fn save(&self, session: &SmartSession) -> Result<(), CliError> {
        self.sessions().save(session)
    }
    fn delete(&self, account: &str) -> Result<bool, CliError> {
        SessionStore::delete(&self.sessions(), account)
    }
}

/// The stored session at `account`, or exit 3 naming the sign-in to redo.
pub fn load(store: &dyn SessionStore, entry: &SessionEntry) -> Result<SmartSession, CliError> {
    let profile = entry.profile.as_deref();
    let found = store.load(&entry.key()).map_err(|e| {
        if e.reason() == Some(Reason::CredentialUnreadable) {
            e.with_remedy(format!(
                "allow athena when the Keychain asks; if the sign-in still does not read, \
                 remove it with `athena --profile {p} auth logout --org {org} --patient \
                 {patient}`, then {again}",
                p = profile_or_placeholder(profile),
                org = entry.org,
                patient = entry.patient,
                again = sign_in_again(profile, &entry.org)
            ))
        } else {
            e
        }
    })?;
    found.ok_or_else(|| {
        CliError::Auth(format!(
            "athena holds no sign-in for {} at {}",
            entry.patient, entry.org
        ))
        .because(Reason::NoCredential)
        .with_remedy(sign_in_again(profile, &entry.org))
    })
}

fn profile_or_placeholder(profile: Option<&str>) -> &str {
    profile.unwrap_or("<NAME>")
}

/// The remedy of a refused or missing sign-in.
pub fn sign_in_again(profile: Option<&str>, org: &str) -> String {
    format!(
        "sign in again with `athena --profile {} auth login --org {org}`",
        profile_or_placeholder(profile)
    )
}

/// A fingerprint of the login that made `session`: the first 12 hex digits
/// of the SHA-256 of its ID token's issuer and subject. It tells two logins
/// apart without naming either. `None` when there is no ID token, or it
/// carries no `iss` or `sub`.
pub fn login_fingerprint(session: &SmartSession) -> Option<String> {
    let claims = pk_cli_auth::token::claims(session.tokens.id_token.as_deref()?)?;
    let iss = claims.get("iss")?.as_str()?;
    let sub = claims.get("sub")?.as_str()?;
    let digest = Sha256::digest(format!("{iss}\n{sub}").as_bytes());
    Some(digest[..6].iter().map(|b| format!("{b:02x}")).collect())
}

/// When the refresh token lapses, by athenahealth's documented 90 days from
/// the last grant (`tokens.obtained_at`).
pub fn refresh_expires_at(session: &SmartSession) -> Option<u64> {
    session
        .tokens
        .refresh_token
        .as_ref()
        .map(|_| session.tokens.obtained_at + REFRESH_LIFETIME_SECS)
}

/// Whether a read can still succeed without a new sign-in: not refused, and
/// an access token in date or a refresh token that is.
pub fn usable(session: &SmartSession, now: u64) -> bool {
    session.refused_at.is_none()
        && (!session.tokens.is_expired(now, DEFAULT_SKEW_SECS)
            || refresh_expires_at(session).is_some_and(|at| at > now))
}

/// When a new sign-in will be needed (Unix seconds): when athenahealth
/// refused the last grant, else the refresh token's 90 days, else the access
/// token's hour.
pub fn signin_expires_at(session: &SmartSession) -> u64 {
    session.refused_at.unwrap_or_else(|| {
        refresh_expires_at(session)
            .or(session.tokens.expires_at)
            .unwrap_or(session.tokens.obtained_at)
    })
}

/// The revocation endpoint discovered at sign-in, if athenahealth named one.
pub fn revocation_endpoint(session: &SmartSession) -> Option<&str> {
    session
        .extra
        .get(REVOCATION_ENDPOINT)
        .and_then(Value::as_str)
}

/// One signed-in patient, as the index records it. No token, no name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// The login profile the sign-in belongs to; `None` for one stored before
    /// v0.4.0, which `athena profiles migrate` moves into a profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub org: String,
    pub patient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `self` when the signed-in person is the patient, `proxy` for a caregiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationship: Option<String>,
    pub environment: Environment,
    pub fhir_base: String,
    pub stored_at: String,
    /// When a new sign-in will be needed (RFC 3339), as of the last token
    /// request athena made: the refresh token's 90 days, or the access token's
    /// hour without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signin_expires_at: Option<String>,
    /// The fingerprint of the login that made the sign-in
    /// ([`login_fingerprint`]), when its ID token named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

impl SessionEntry {
    /// The sign-in's account: its vault entry within its profile, or its
    /// keychain account before v0.4.0.
    pub fn key(&self) -> String {
        key(&self.org, &self.patient)
    }

    fn is(&self, profile: Option<&str>, org: &str, patient: &str) -> bool {
        self.profile.as_deref() == profile && self.org == org && self.patient == patient
    }
}

/// The session index, keyed by profile, practice profile and patient.
pub const INDEX_FILE: &str = "sessions.v2.json";
/// The index of v0.3.x and before, keyed by practice profile and patient.
/// v0.4.0 imports it once, as sign-ins in no profile, and never writes it, so
/// a v0.3.x binary run again finds its own index as it left it.
pub const V1_INDEX_FILE: &str = "sessions.json";
/// The version [`INDEX_FILE`] is written in.
pub const INDEX_VERSION: u32 = 2;

/// [`INDEX_FILE`]: which token sets exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionIndex {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub sessions: Vec<SessionEntry>,
}

impl Default for SessionIndex {
    fn default() -> Self {
        SessionIndex {
            version: INDEX_VERSION,
            sessions: Vec::new(),
        }
    }
}

impl SessionIndex {
    /// The index at `path`; empty when there is none. One written by a newer
    /// athena is refused rather than read in this one's shape and written
    /// back.
    pub fn load(path: &Path) -> Result<SessionIndex, CliError> {
        let index: SessionIndex = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                CliError::Other(format!("{} is not a session index: {e}", path.display()))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(CliError::Other(format!("reading {}: {e}", path.display()))),
        };
        if index.version > INDEX_VERSION {
            return Err(CliError::Other(format!(
                "{} was written by a newer athena (index version {}); this one reads {}",
                path.display(),
                index.version,
                INDEX_VERSION
            ))
            .with_remedy("update athena with `athena self-update`"));
        }
        Ok(index)
    }

    /// The index in `dir`: [`INDEX_FILE`], or, until that exists, the
    /// sign-ins of v0.3.x's [`V1_INDEX_FILE`], each in no profile.
    pub fn load_in(dir: &Path) -> Result<SessionIndex, CliError> {
        let path = dir.join(INDEX_FILE);
        if path.exists() {
            return Self::load(&path);
        }
        let mut old = Self::load(&dir.join(V1_INDEX_FILE))?;
        for entry in &mut old.sessions {
            entry.profile = None;
            entry.login = None;
        }
        old.version = INDEX_VERSION;
        Ok(old)
    }

    /// Write owner-only, through a temporary file renamed into place, in
    /// [`INDEX_VERSION`].
    pub fn save(&self, path: &Path) -> Result<(), CliError> {
        let current = SessionIndex {
            version: INDEX_VERSION,
            sessions: self.sessions.clone(),
        };
        let text = serde_json::to_string_pretty(&current)
            .map_err(|e| CliError::Other(format!("encoding the session index: {e}")))?;
        write_private(path, text.as_bytes())
    }

    /// Add or replace the entry for (profile, org, patient).
    pub fn upsert(&mut self, entry: SessionEntry) {
        self.sessions
            .retain(|s| !s.is(entry.profile.as_deref(), &entry.org, &entry.patient));
        self.sessions.push(entry);
    }

    /// The entry for (profile, org, patient), to change in place.
    pub fn find_mut(
        &mut self,
        profile: Option<&str>,
        org: &str,
        patient: &str,
    ) -> Option<&mut SessionEntry> {
        self.sessions
            .iter_mut()
            .find(|s| s.is(profile, org, patient))
    }

    /// Give the sign-in labelled `from` at `org` in `profile` the label `to`,
    /// refusing a label another sign-in there already has.
    pub fn relabel(
        &mut self,
        profile: &str,
        org: &str,
        from: &str,
        to: &str,
    ) -> Result<SessionEntry, CliError> {
        if from != to && self.label_taken(Some(profile), org, to, None) {
            return Err(CliError::Usage(format!(
                "the label `{to}` already names another sign-in at `{org}` in profile `{profile}`"
            ))
            .with_remedy(format!(
                "relabel or sign out that one first (`athena --profile {profile} patients list`)"
            )));
        }
        let Some(entry) = self.sessions.iter_mut().find(|s| {
            s.profile.as_deref() == Some(profile)
                && s.org == org
                && s.label.as_deref() == Some(from)
        }) else {
            return Err(CliError::NotFound(format!(
                "no sign-in at `{org}` in profile `{profile}` has the label `{from}`"
            ))
            .with_remedy(format!(
                "list them with `athena --profile {profile} patients list`"
            )));
        };
        entry.label = Some(to.to_string());
        Ok(entry.clone())
    }

    /// Whether a sign-in at `org` in `profile`, other than `except`'s patient,
    /// has the label `label`.
    pub fn label_taken(
        &self,
        profile: Option<&str>,
        org: &str,
        label: &str,
        except: Option<&str>,
    ) -> bool {
        self.sessions.iter().any(|s| {
            s.profile.as_deref() == profile
                && s.org == org
                && s.label.as_deref() == Some(label)
                && except != Some(s.patient.as_str())
        })
    }

    pub fn remove(&mut self, profile: Option<&str>, org: &str, patient: &str) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| !s.is(profile, org, patient));
        before != self.sessions.len()
    }

    /// The entries of `profile` that `--org` and `--patient` select (each
    /// optional). `profile` `None` selects the sign-ins stored before v0.4.0,
    /// which are in no profile.
    pub fn matching(
        &self,
        profile: Option<&str>,
        patient: Option<&str>,
        org: Option<&str>,
    ) -> Vec<&SessionEntry> {
        self.sessions
            .iter()
            .filter(|s| s.profile.as_deref() == profile)
            .filter(|s| org.is_none_or(|o| s.org == o))
            .filter(|s| patient.is_none_or(|p| s.patient == p || s.label.as_deref() == Some(p)))
            .collect()
    }

    /// How many sign-ins are in no profile (stored before v0.4.0).
    pub fn unassigned(&self) -> usize {
        self.sessions.iter().filter(|s| s.profile.is_none()).count()
    }

    /// Exactly one signed-in patient of `profile`, by id or label, optionally
    /// within one practice profile. With no `--patient`, the only one there
    /// is.
    pub fn resolve(
        &self,
        profile: &str,
        patient: Option<&str>,
        org: Option<&str>,
    ) -> Result<SessionEntry, CliError> {
        let found = self.matching(Some(profile), patient, org);
        let list = format!("list them with `athena --profile {profile} patients list`");
        match (found.len(), patient) {
            (1, _) => Ok(found[0].clone()),
            (0, None) if self.matching(Some(profile), None, None).is_empty() => Err(
                CliError::Auth(format!("no patient is signed in in profile `{profile}`"))
                    .because(Reason::NoCredential)
                    .with_remedy(self.no_sign_in_remedy(profile)),
            ),
            (0, Some(p)) => Err(CliError::NotFound(format!(
                "no signed-in patient in profile `{profile}` has the id or label `{p}`{}",
                org.map(|o| format!(" at `{o}`")).unwrap_or_default()
            ))
            .with_remedy(list)),
            (0, None) => Err(CliError::NotFound(format!(
                "no patient is signed in at `{}` in profile `{profile}`",
                org.unwrap_or_default()
            ))
            .with_remedy(list)),
            (_, _) => Err(CliError::Usage(
                "more than one signed-in patient matches: name one with --patient <ID|LABEL> \
                 and, if needed, --org <ORG>"
                    .into(),
            )),
        }
    }

    /// What to do when `profile` has no sign-in: move the ones from before
    /// v0.4.0 into it, when there are any, else sign in.
    fn no_sign_in_remedy(&self, profile: &str) -> String {
        if self.unassigned() > 0 {
            format!(
                "move a sign-in stored before v0.4.0 into it with `athena profiles migrate \
                 {profile} --org <ORG> --patient <ID|LABEL>`, or sign in with `athena --profile \
                 {profile} auth login --org <ORG>`"
            )
        } else {
            format!("sign in with `athena --profile {profile} auth login --org <ORG>`")
        }
    }
}

/// Write `bytes` to `path` readable by the owner alone, replacing it whole.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| CliError::Other(format!("{} has no directory", path.display())))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| CliError::Other(format!("creating {}: {e}", dir.display())))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|e| CliError::Other(format!("writing {}: {e}", tmp.display())))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| CliError::Other(format!("writing {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| CliError::Other(format!("replacing {}: {e}", path.display())))
}

/// A synthetic session, for tests in this crate.
#[cfg(test)]
pub(crate) fn test_session(
    org: &str,
    patient: &str,
    revocation: Option<&str>,
    now: u64,
) -> SmartSession {
    let mut extra = serde_json::Map::new();
    if let Some(endpoint) = revocation {
        extra.insert(REVOCATION_ENDPOINT.into(), endpoint.into());
    }
    serde_json::from_value(serde_json::json!({
        "format": 1,
        "organization": org,
        "patient": patient,
        "fhir_base": "http://127.0.0.1:9/fhir/r4",
        "token_endpoint": "http://127.0.0.1:9/oauth2/v1/token",
        "client_id": "client",
        "scope": "openid",
        "tokens": {
            "access_token": "access-1",
            "token_type": "Bearer",
            "refresh_token": null,
            "expires_at": now + 3600,
            "scope": "openid",
            "obtained_at": now,
        },
        "extra": extra,
    }))
    .expect("a synthetic session")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(org: &str, patient: &str, label: Option<&str>) -> SessionEntry {
        in_profile("adult1", org, patient, label)
    }

    fn in_profile(profile: &str, org: &str, patient: &str, label: Option<&str>) -> SessionEntry {
        SessionEntry {
            profile: Some(profile.into()),
            org: org.into(),
            patient: patient.into(),
            label: label.map(Into::into),
            relationship: None,
            environment: Environment::Preview,
            fhir_base: "https://example.invalid/fhir/r4".into(),
            stored_at: "2026-01-01T00:00:00Z".into(),
            signin_expires_at: None,
            login: None,
        }
    }

    #[test]
    fn a_patient_resolves_by_id_or_label_within_its_profile() {
        let mut index = SessionIndex::default();
        index.upsert(entry("clinic", "a-1.E-1", Some("kid-one")));
        index.upsert(entry("clinic", "a-1.E-2", Some("kid-two")));
        index.upsert(in_profile("adult2", "clinic", "a-1.E-4", Some("kid-one")));
        assert_eq!(
            index
                .resolve("adult1", Some("kid-two"), None)
                .unwrap()
                .patient,
            "a-1.E-2"
        );
        assert_eq!(
            index
                .resolve("adult1", Some("kid-one"), Some("clinic"))
                .unwrap()
                .patient,
            "a-1.E-1"
        );
        assert_eq!(
            index
                .resolve("adult2", Some("kid-one"), None)
                .unwrap()
                .patient,
            "a-1.E-4"
        );
        assert_eq!(
            index.resolve("adult2", None, None).unwrap().patient,
            "a-1.E-4"
        );
        assert_eq!(
            index.resolve("adult1", None, None).unwrap_err().exit_code(),
            2
        );
        assert_eq!(
            index
                .resolve("adult1", Some("a-1.E-4"), None)
                .unwrap_err()
                .exit_code(),
            4
        );
        let none = index.resolve("adult3", None, None).unwrap_err();
        assert_eq!(none.exit_code(), 3);
        assert!(
            none.remedy()
                .unwrap()
                .contains("athena --profile adult3 auth login"),
            "{none}"
        );
    }

    #[test]
    fn sign_ins_from_before_profiles_are_in_none_and_named_in_the_remedy() {
        let mut index = SessionIndex::default();
        let mut old = entry("clinic", "a-1.E-1", Some("kid"));
        old.profile = None;
        index.upsert(old);
        assert_eq!(index.unassigned(), 1);
        assert!(index.matching(Some("adult1"), None, None).is_empty());
        assert_eq!(index.matching(None, Some("kid"), None).len(), 1);
        let err = index.resolve("adult1", None, None).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(
            err.remedy()
                .unwrap()
                .contains("athena profiles migrate adult1 --org <ORG>"),
            "{err}"
        );
    }

    #[test]
    fn upsert_replaces_the_same_patient_in_the_same_profile_only() {
        let mut index = SessionIndex::default();
        index.upsert(entry("clinic", "a-1.E-1", Some("old")));
        index.upsert(entry("clinic", "a-1.E-1", Some("new")));
        index.upsert(in_profile("adult2", "clinic", "a-1.E-1", Some("kid")));
        assert_eq!(index.sessions.len(), 2);
        assert_eq!(
            index.matching(Some("adult1"), None, None)[0]
                .label
                .as_deref(),
            Some("new")
        );
        assert!(index
            .find_mut(Some("adult2"), "clinic", "a-1.E-1")
            .is_some());
        assert!(index.remove(Some("adult1"), "clinic", "a-1.E-1"));
        assert!(!index.remove(Some("adult1"), "clinic", "a-1.E-1"));
        assert_eq!(index.sessions.len(), 1);
    }

    #[test]
    fn relabel_renames_one_sign_in_and_keeps_labels_distinct_per_profile() {
        let mut index = SessionIndex::default();
        index.upsert(entry("clinic", "a-1.E-1", Some("draft")));
        index.upsert(entry("clinic", "a-1.E-2", Some("kid")));
        index.upsert(entry("other", "a-1.E-3", Some("final")));
        index.upsert(in_profile("adult2", "clinic", "a-1.E-5", Some("kid")));
        let renamed = index.relabel("adult1", "clinic", "draft", "final").unwrap();
        assert_eq!(renamed.patient, "a-1.E-1");
        assert_eq!(
            index
                .matching(Some("adult1"), Some("final"), Some("clinic"))
                .len(),
            1
        );
        assert!(index
            .matching(Some("adult1"), Some("draft"), None)
            .is_empty());
        let taken = index
            .relabel("adult1", "clinic", "final", "kid")
            .unwrap_err();
        assert_eq!(taken.exit_code(), 2);
        assert!(index.relabel("adult1", "clinic", "kid", "kid").is_ok());
        let missing = index
            .relabel("adult1", "clinic", "nobody", "x")
            .unwrap_err();
        assert_eq!(missing.exit_code(), 4);
        // Another profile's label is its own.
        let elsewhere = index.relabel("adult2", "clinic", "draft", "x");
        assert_eq!(elsewhere.unwrap_err().exit_code(), 4);
        assert!(index.relabel("adult2", "clinic", "kid", "renamed").is_ok());
    }

    #[test]
    fn the_login_fingerprint_tells_logins_apart_without_naming_one() {
        fn id_token(claims: serde_json::Value) -> String {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            format!(
                "{}.{}.sig",
                b64.encode(br#"{"alg":"RS256"}"#),
                b64.encode(claims.to_string())
            )
        }
        let mut s = test_session("clinic", "a-1.E-1", None, 0);
        assert_eq!(login_fingerprint(&s), None);
        s.tokens.id_token = Some(id_token(
            serde_json::json!({"iss": "https://idp.example.test", "sub": "user-1"}),
        ));
        let one = login_fingerprint(&s).unwrap();
        assert_eq!(one.len(), 12);
        assert!(one.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!one.contains("user-1"));
        s.tokens.id_token = Some(id_token(
            serde_json::json!({"iss": "https://idp.example.test", "sub": "user-1", "nonce": "n"}),
        ));
        assert_eq!(login_fingerprint(&s).unwrap(), one);
        s.tokens.id_token = Some(id_token(
            serde_json::json!({"iss": "https://idp.example.test", "sub": "user-2"}),
        ));
        assert_ne!(login_fingerprint(&s).unwrap(), one);
        s.tokens.id_token = Some(id_token(serde_json::json!({"sub": "user-1"})));
        assert_eq!(login_fingerprint(&s), None);
    }

    #[test]
    fn each_profile_keeps_its_own_sign_ins_in_the_vault() {
        use pk_cli_auth::vault::MemoryKeys;
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::new(MemoryKeys::new(), dir.path().join("vault")));
        let adult1 = profile_store(vault.clone(), ProfileName::parse("adult1").unwrap());
        let adult2 = profile_store(vault.clone(), ProfileName::parse("adult2").unwrap());
        let s = test_session("clinic", "a-1.E-1", None, 0);
        adult1.save(&s).unwrap();
        assert!(adult1.load(&s.account()).unwrap().is_some());
        assert!(adult2.load(&s.account()).unwrap().is_none());
        assert_eq!(
            vault.names().unwrap(),
            vec!["adult1/clinic/a-1.E-1".to_string()]
        );
        assert!(adult1.delete(&s.account()).unwrap());
        assert!(vault.names().unwrap().is_empty());
    }

    #[test]
    fn usability_follows_the_refresh_token_and_a_refusal() {
        let now = 1_000_000;
        let mut s = test_session("clinic", "a-1.E-1", None, now);
        s.tokens.expires_at = Some(now - 1);
        assert!(!usable(&s, now));
        assert_eq!(signin_expires_at(&s), now - 1);
        s.tokens.refresh_token = Some("r".into());
        assert!(usable(&s, now));
        assert!(!usable(&s, now + REFRESH_LIFETIME_SECS + 1));
        assert_eq!(signin_expires_at(&s), now + REFRESH_LIFETIME_SECS);
        s.refused_at = Some(now + 5);
        assert!(!usable(&s, now));
        assert_eq!(signin_expires_at(&s), now + 5);
    }

    #[test]
    fn the_account_is_pk_med_smarts() {
        let s = test_session("clinic", "a-1.E-1", None, 0);
        assert_eq!(s.account(), "smart/clinic/a-1.E-1");
        assert_eq!(entry("clinic", "a-1.E-1", None).key(), s.account());
        let store = pk_med_smart::MemoryStore::default();
        store.save(&s).unwrap();
        assert!(store.load("smart/clinic/a-1.E-1").unwrap().is_some());
        assert!(store.delete("smart/clinic/a-1.E-1").unwrap());
    }

    #[test]
    fn an_item_that_does_not_read_names_athenas_commands() {
        struct Unreadable;
        impl SessionStore for Unreadable {
            fn load(&self, _: &str) -> Result<Option<SmartSession>, CliError> {
                Err(CliError::Keychain("the item does not read".into())
                    .because(Reason::CredentialUnreadable)
                    .with_remedy(pk_med_smart::remedy::CLEAR_AND_SIGN_IN_AGAIN))
            }
            fn save(&self, _: &SmartSession) -> Result<(), CliError> {
                Ok(())
            }
            fn delete(&self, _: &str) -> Result<bool, CliError> {
                Ok(false)
            }
        }
        let err = load(&Unreadable, &entry("clinic", "a-1.E-1", None)).unwrap_err();
        assert_eq!(err.reason(), Some(Reason::CredentialUnreadable));
        let remedy = err.remedy().unwrap();
        assert!(
            remedy.contains("athena --profile adult1 auth logout --org clinic --patient a-1.E-1")
                && remedy.contains("athena --profile adult1 auth login --org clinic"),
            "{remedy}"
        );
    }

    #[test]
    fn the_revocation_endpoint_rides_in_extra() {
        let s = test_session(
            "clinic",
            "a-1.E-1",
            Some("https://example.invalid/revoke"),
            0,
        );
        assert_eq!(
            revocation_endpoint(&s),
            Some("https://example.invalid/revoke")
        );
        let text = serde_json::to_string(&s).unwrap();
        let back: SmartSession = serde_json::from_str(&text).unwrap();
        assert_eq!(
            revocation_endpoint(&back),
            Some("https://example.invalid/revoke")
        );
    }

    #[test]
    fn v0_3_s_index_is_imported_in_no_profile_and_left_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join(V1_INDEX_FILE);
        let text = serde_json::json!({"sessions": [{
            "org": "clinic",
            "patient": "a-1.E-1",
            "label": "kid",
            "environment": "production",
            "fhir_base": "https://example.invalid/fhir/r4",
            "stored_at": "2026-01-01T00:00:00Z"
        }]})
        .to_string();
        std::fs::write(&v1, &text).unwrap();
        let index = SessionIndex::load_in(dir.path()).unwrap();
        assert_eq!(index.unassigned(), 1);
        assert_eq!(index.version, INDEX_VERSION);
        // Writing goes to the new file; v0.3.x's stays as it was.
        let mut index = index;
        index.upsert(entry("clinic", "a-1.E-2", Some("other")));
        index.save(&dir.path().join(INDEX_FILE)).unwrap();
        assert_eq!(std::fs::read_to_string(&v1).unwrap(), text);
        let again = SessionIndex::load_in(dir.path()).unwrap();
        assert_eq!(again, index);
        // Once the new file exists, the old one is not read again.
        std::fs::write(&v1, r#"{"sessions": []}"#).unwrap();
        assert_eq!(SessionIndex::load_in(dir.path()).unwrap().sessions.len(), 2);
    }

    #[test]
    fn an_index_from_a_newer_athena_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INDEX_FILE);
        std::fs::write(&path, r#"{"version": 3, "sessions": []}"#).unwrap();
        let err = SessionIndex::load(&path).unwrap_err();
        assert!(err.to_string().contains("newer athena"), "{err}");
        let saved = serde_json::to_value(SessionIndex::default()).unwrap();
        assert_eq!(saved["version"], INDEX_VERSION);
    }

    #[test]
    fn the_index_is_written_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut index = SessionIndex::default();
        index.upsert(entry("clinic", "a-1.E-1", None));
        index.save(&path).unwrap();
        assert_eq!(SessionIndex::load(&path).unwrap(), index);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
