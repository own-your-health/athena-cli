//! What to enter in athenahealth's Developer Portal to register an app for
//! athena (`athena registration`, medical-common's `registration/v1`), and
//! whether a client id is configured (the `client` member of `info` and
//! `auth status`). The values follow `docs/registration.md`; the two change
//! together.

use pk_cli_core::CliError;
use pk_med_smart::{ClientStatus, ClientType, Registration, ScopeVersion, StoredIn};
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::env::Environment;
use crate::{smart, BIN};

/// athenahealth's Developer Portal, where an app is registered.
pub const PORTAL_URL: &str = "https://developer.api.athena.io/ams-portal/";
/// The step-by-step guide, as `registration/v1` names it: its path in the
/// repository.
pub const DOC: &str = "docs/registration.md";
/// The same guide as a URL, for a binary installed from a release, which has
/// no checkout to find the path in.
pub const DOC_URL: &str = "https://github.com/piekstra/athena-cli/blob/main/docs/registration.md";

/// The environments athenahealth registers apps in. A FHIR server of its own
/// (`--env custom`) is registered with whoever runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RegisterEnv {
    Preview,
    #[default]
    Production,
}

impl From<RegisterEnv> for Environment {
    fn from(env: RegisterEnv) -> Self {
        match env {
            RegisterEnv::Preview => Environment::Preview,
            RegisterEnv::Production => Environment::Production,
        }
    }
}

/// The command that stores the client id the portal issued.
pub fn store_command(env: Environment) -> String {
    format!("{BIN} config set client-id <CLIENT_ID> --env {env}")
}

/// The registration for `env`, with this configuration's redirect ports.
/// athenahealth registers Preview and Production apps; `custom` gets
/// Production's form.
pub fn form(config: &Config, env: Environment) -> Registration {
    let preview = env == Environment::Preview;
    let env = if preview {
        Environment::Preview
    } else {
        Environment::Production
    };
    let mut reg = Registration::new(
        BIN,
        "athenahealth",
        PORTAL_URL,
        DOC,
        ClientType::Public,
        store_command(env),
    );
    for port in config.redirect_ports() {
        reg = reg.redirect_uri(format!("http://127.0.0.1:{port}/callback"));
    }
    reg.scopes(&smart::scopes(ScopeVersion::V2))
        .field_with_note(
            "Create a New App",
            "My app will use Certified APIs ONLY",
            "then the category and the box below, and Continue to App Details",
        )
        .field("App category", "3-Legged OAuth for Patients")
        .field("PHR app", "tick \"I confirm that I am building a PHR app\"")
        .field_with_note(
            "Environment",
            if preview { "Preview" } else { "Production" },
            if preview {
                "athenahealth's sandbox and its synthetic patient only"
            } else {
                "real practices; `athena registration --env preview` gives the sandbox's app"
            },
        )
        .field_with_note(
            "App name",
            "<your brand> Health Records",
            "your own brand plus what the app does",
        )
        .url("Website", "https://github.com/piekstra/athena-cli")
        .field(
            "Description",
            "Command-line tool with which a patient or caregiver downloads their own records \
             from athenahealth practices to their own computer.",
        )
        .field_with_note(
            "Intended end users",
            "The patient and approved family members or caregivers",
            "a caregiver signs in for a child",
        )
        .field("OAuth client type", "Public (native)")
        .field("Token authentication", "PKCE, no client secret")
        .field_with_note(
            "Redirect URLs",
            "each of the Redirect URIs above",
            "exactly, port included",
        )
        .note(
            "On the app's Scopes tab, open FHIR R4 SMART V2, tick read (.r) and search \
             (.s) for each patient/<Type>.rs scope above, and choose Confirm Scopes. \
             athenahealth grants openid, fhirUser, launch/patient and offline_access to every \
             patient app, with no box to tick.",
        )
        .note(
            "The portal refuses an app name with \"athena\", \"athenahealth\", \"app\", \
             \"application\", \"PHR\", \"CAPI\", \"test\", environment words or people's names, \
             and a name another app has: add a word.",
        )
        .note(
            "There is no review: the client id is issued when the app is created, and a \
             Production app reads at any athenaOne practice once a patient consents.",
        )
        .note(
            "A new client id can take up to about 18 hours to reach athenahealth's API \
             gateway. Until then sign-in and consent work, then the token request answers 401 \
             InvalidClient: athena exits 5, stores nothing, and works on a later sign-in.",
        )
        .note(format!("The guide: {DOC_URL}"))
}

/// A command that needs a client id for `env` and finds none (exit 2,
/// `pk_med_smart::registration::NO_CLIENT`).
pub fn missing_client(config: &Config, env: Environment) -> CliError {
    let reg = form(config, env);
    let error = pk_med_smart::missing_client(&reg, Some(env.as_str()));
    match env {
        Environment::Production => error,
        // `athena registration` alone prints Production's form.
        Environment::Preview => error.with_remedy(format!(
            "register one as {DOC} describes, entering what `{BIN} registration --env preview` \
             prints, then run `{}`",
            reg.store
        )),
        Environment::Custom => error.with_remedy(format!(
            "register an app with whoever runs this FHIR server ({DOC_URL}), then run `{BIN} \
             config set client-id <CLIENT_ID> --env custom`"
        )),
    }
}

/// The last four characters of a client id: enough to tell two apart, while
/// `info` and `auth status` output, which gets pasted into bug reports,
/// carries no whole id. athenahealth's terms keep the Keys it issues to the
/// registrant (`docs/api.md`). `config show` prints the id whole.
pub fn ends_with(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// The Production registration's client, the one a patient's own records
/// need, and its id's last four characters. `orgs list` shows each practice
/// profile's, the sandbox's included.
pub fn status(config: &Config) -> (ClientStatus, Option<String>) {
    let production = Environment::Production.as_str();
    match config.client_ids.get(Environment::Production) {
        Some(id) => (
            ClientStatus::configured(ClientType::Public, StoredIn::Config).registration(production),
            Some(ends_with(id)),
        ),
        None => (ClientStatus::none().registration(production), None),
    }
}

/// `dto` as JSON with medical-common's `client` member, carrying athena's
/// `client_id_ends_with` in place of the whole id.
pub fn with_client<T: Serialize>(dto: &T, config: &Config) -> Result<Value, CliError> {
    let (status, tail) = status(config);
    let mut value = pk_med_smart::with_client(dto, &status)?;
    if let Some(tail) = tail {
        value["client"]["client_id_ends_with"] = json!(tail);
    }
    Ok(value)
}

/// The human line for `auth status`: medical-common's line without its
/// `client: ` label, and the id's last four characters.
pub fn status_line(config: &Config) -> String {
    let (status, tail) = status(config);
    let text = status.text(&form(config, Environment::Production));
    let text = text.strip_prefix("client: ").unwrap_or(&text);
    match tail {
        Some(tail) => format!("{text}, id ending {tail}"),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_lists_the_configured_redirect_ports() {
        let config = Config {
            redirect_ports: Some(vec![50123, 50124]),
            ..Config::default()
        };
        let reg = form(&config, Environment::Production);
        assert_eq!(
            reg.redirect_uris,
            [
                "http://127.0.0.1:50123/callback",
                "http://127.0.0.1:50124/callback"
            ]
        );
        assert_eq!(
            reg.store,
            "athena config set client-id <CLIENT_ID> --env production"
        );
        assert_eq!(reg.doc, "docs/registration.md");
    }

    #[test]
    fn the_form_asks_for_every_type_a_sign_in_requests() {
        let reg = form(&Config::default(), Environment::Preview);
        for ty in smart::PATIENT_TYPES {
            assert!(reg.scopes.contains(&format!("patient/{ty}.rs")), "{ty}");
        }
        let env = reg
            .fields
            .iter()
            .find(|f| f.label == "Environment")
            .unwrap();
        assert_eq!(env.value, "Preview");
        assert!(reg.store.ends_with("--env preview"));
    }

    #[test]
    fn a_client_id_is_shown_by_its_last_four_characters() {
        let mut config = Config::default();
        config
            .client_ids
            .set(Environment::Production, Some("0oaEXAMPLEclient9876".into()));
        let value = with_client(&json!({"schema": "cli-info/v1"}), &config).unwrap();
        assert_eq!(value["client"]["configured"], true);
        assert_eq!(value["client"]["registration"], "production");
        assert_eq!(value["client"]["client_type"], "public");
        assert_eq!(value["client"]["stored_in"], "config");
        assert_eq!(value["client"]["client_id_ends_with"], "9876");
        assert!(!value.to_string().contains("0oaEXAMPLE"));
        assert!(status_line(&config).ends_with("id ending 9876"));
        assert_eq!(ends_with("ab"), "ab");

        let value = with_client(&json!({}), &Config::default()).unwrap();
        assert_eq!(value["client"]["configured"], false);
        assert!(value["client"].get("client_id_ends_with").is_none());
    }

    #[test]
    fn a_missing_client_names_the_registration_and_store_commands() {
        let err = missing_client(&Config::default(), Environment::Preview);
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.reason(), Some(pk_med_smart::registration::NO_CLIENT));
        let text = format!("{} {}", err.message(), err.remedy().unwrap_or_default());
        for want in [
            "preview",
            "`athena registration --env preview`",
            "docs/registration.md",
            "athena config set client-id <CLIENT_ID> --env preview",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
    }
}
