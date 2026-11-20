//! What to enter in athenahealth's Developer Portal to register an app for
//! athena (`athena registration`, medical-common's `registration/v2`), one
//! registration per environment, and whether each has a client id (the
//! `clients` member of `info` and `auth status`). The values follow
//! `docs/registration.md`; the two change together.

use pk_cli_core::CliError;
use pk_med_smart::{ClientStatus, ClientType, Registration, ScopeVersion, StoredIn};
use serde::Serialize;
use serde_json::Value;

use crate::config::Config;
use crate::env::Environment;
use crate::{smart, BIN};

/// athenahealth's Developer Portal, where an app is registered.
pub const PORTAL_URL: &str = "https://developer.api.athena.io/ams-portal/";
/// The step-by-step guide, as a URL: a binary installed from a release has
/// no checkout to find `docs/registration.md` in.
pub const DOC_URL: &str = "https://github.com/piekstra/athena-cli/blob/main/docs/registration.md";

/// The environments with a registration, in the order `info` lists them.
pub const ENVIRONMENTS: [Environment; 2] = [Environment::Production, Environment::Preview];

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
        DOC_URL,
        ClientType::Public,
        store_command(env),
    )
    .registration(env.as_str());
    if preview {
        reg = reg.command(format!("{BIN} registration --env preview"));
    }
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
        .selection_with_note(
            "Scopes tab: FHIR R4 SMART V2",
            smart::PATIENT_TYPES.iter().copied(),
            "tick read (.r) and search (.s) for each type, then choose Confirm Scopes; \
             athenahealth grants openid, fhirUser, launch/patient and offline_access to every \
             patient app, with no box to tick",
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
}

/// A command that needs a client id for `env` and finds none (exit 2,
/// `pk_med_smart::registration::NO_CLIENT`). A custom FHIR server has no
/// athenahealth registration, so its remedy names none.
pub fn missing_client(config: &Config, env: Environment) -> CliError {
    let error = pk_med_smart::missing_client(&form(config, env));
    if env == Environment::Custom {
        return error.with_remedy(format!(
            "register an app with whoever runs this FHIR server ({DOC_URL}), then run `{BIN} \
             config set client-id <CLIENT_ID> --env custom`"
        ));
    }
    error
}

/// A client id's last four characters, as `clients` shows them: none for an
/// id under 12 characters (medical-common's rule). athenahealth's terms keep
/// the Keys it issues to the registrant (`docs/api.md`), and `auth status`
/// output gets pasted into bug reports, so no output but `config show`
/// carries an id whole.
pub fn ends_with(id: &str) -> Option<String> {
    ClientStatus::none()
        .client_id_last_four(id)
        .client_id_ends_with
}

/// Each environment's registration with whether it has a client id. A
/// practice profile with its own client id is in `orgs list`.
pub fn statuses(config: &Config) -> Vec<ClientStatus> {
    ENVIRONMENTS
        .iter()
        .map(|&env| {
            let id = config.client_ids.get(env);
            let status = ClientStatus::of(&form(config, env), id.map(|_| StoredIn::Config));
            match id {
                Some(id) => status.client_id_last_four(id),
                None => status,
            }
        })
        .collect()
}

/// `dto` as JSON with medical-common's `clients` member.
pub fn with_clients<T: Serialize>(dto: &T, config: &Config) -> Result<Value, CliError> {
    pk_med_smart::with_clients(dto, &statuses(config))
}

/// The human lines for `auth status`: one per registration.
pub fn status_lines(config: &Config) -> Vec<String> {
    ENVIRONMENTS
        .iter()
        .zip(statuses(config))
        .map(|(&env, status)| status.text(&form(config, env)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(reg.doc, DOC_URL);
        assert_eq!(reg.registration.as_deref(), Some("production"));
        assert_eq!(reg.command, "athena registration");
    }

    #[test]
    fn the_form_asks_for_every_type_a_sign_in_requests() {
        let reg = form(&Config::default(), Environment::Preview);
        for ty in smart::PATIENT_TYPES {
            assert!(reg.scopes.contains(&format!("patient/{ty}.rs")), "{ty}");
        }
        assert_eq!(reg.selections[0].items.len(), smart::PATIENT_TYPES.len());
        let env = reg
            .fields
            .iter()
            .find(|f| f.label == "Environment")
            .unwrap();
        assert_eq!(env.value, "Preview");
        assert_eq!(reg.registration.as_deref(), Some("preview"));
        assert_eq!(reg.command, "athena registration --env preview");
        assert!(reg.store.ends_with("--env preview"));
    }

    #[test]
    fn each_environment_shows_its_client_by_its_last_four_characters() {
        let mut config = Config::default();
        config
            .client_ids
            .set(Environment::Production, Some("0oaEXAMPLEclient9876".into()));
        let value = with_clients(&json!({"schema": "cli-info/v1"}), &config).unwrap();
        assert_eq!(
            value["clients"],
            json!([
                {"registration": "production", "configured": true, "client_type": "public",
                 "stored_in": "config", "client_id_ends_with": "9876"},
                {"registration": "preview", "configured": false}
            ])
        );
        assert!(!value.to_string().contains("0oaEXAMPLE"));
        let lines = status_lines(&config);
        assert_eq!(
            lines[0],
            "client (production): public, ending 9876, in config"
        );
        assert!(
            lines[1].contains("none configured") && lines[1].contains("--env preview"),
            "{}",
            lines[1]
        );
        assert_eq!(ends_with("0oaEXAMPLEclient9876").as_deref(), Some("9876"));
        assert_eq!(ends_with("short"), None);
    }

    #[test]
    fn a_missing_client_names_the_registration_and_store_commands() {
        let err = missing_client(&Config::default(), Environment::Preview);
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.reason(), Some(pk_med_smart::registration::NO_CLIENT));
        let text = format!("{} {}", err.message(), err.remedy().unwrap_or_default());
        for want in [
            "for the preview registration",
            "`athena registration --env preview`",
            DOC_URL,
            "athena config set client-id <CLIENT_ID> --env preview",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
    }

    #[test]
    fn production_keeps_the_shared_remedy_and_custom_names_no_registration() {
        let err = missing_client(&Config::default(), Environment::Production);
        let remedy = err.remedy().unwrap_or_default();
        assert!(remedy.contains("`athena registration` prints"), "{remedy}");
        assert!(remedy.contains("--env production"), "{remedy}");

        let err = missing_client(&Config::default(), Environment::Custom);
        assert_eq!(err.reason(), Some(pk_med_smart::registration::NO_CLIENT));
        let remedy = err.remedy().unwrap_or_default();
        assert!(remedy.contains("--env custom"), "{remedy}");
        assert!(!remedy.contains("athena registration"), "{remedy}");
    }
}
