//! What to enter in athenahealth's Developer Portal to register an app for
//! athena (`athena registration`, `registration/v1`), and whether a client id
//! is configured (`info`, `auth status`). The values follow
//! `docs/registration.md`; the two change together.

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::env::Environment;
use crate::smart;

pub const SCHEMA: &str = "registration/v1";
/// athenahealth's Developer Portal, where an app is registered.
pub const PORTAL_URL: &str = "https://developer.api.athena.io/ams-portal/";
/// The step-by-step guide, as a URL: a binary installed from a release has
/// no checkout to find `docs/registration.md` in.
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

/// The command that prints the form's values for `env`.
pub fn command(env: Environment) -> String {
    format!("athena registration --env {env}")
}

/// The command that stores the client id the portal issued.
pub fn store_command(env: Environment) -> String {
    format!("athena config set client-id <CLIENT_ID> --env {env}")
}

#[derive(Debug, Serialize)]
pub struct Field {
    pub field: &'static str,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Scopes {
    /// The product to open on the app's Scopes tab.
    pub product: &'static str,
    /// The boxes to tick for each type.
    pub permissions: [&'static str; 2],
    pub types: Vec<&'static str>,
    /// The scope string `auth login` asks for, to compare with the grant.
    pub requested: String,
}

/// `registration/v1`: the portal's choices and form, in the order it asks.
#[derive(Debug, Serialize)]
pub struct Form {
    pub schema: &'static str,
    pub vendor: &'static str,
    pub environment: Environment,
    pub portal: &'static str,
    pub doc: &'static str,
    /// The choices after "(+) Create a New App", in order.
    pub app_type: [&'static str; 3],
    pub fields: Vec<Field>,
    pub launch: &'static str,
    pub client_type: &'static str,
    pub client_secret: bool,
    pub pkce: &'static str,
    pub redirect_uris: Vec<String>,
    pub scopes: Scopes,
    pub store: String,
    /// Whether a client id is configured for this environment already.
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_ends_with: Option<String>,
    /// How long a new client id can take to work.
    pub propagation: &'static str,
}

const PROPAGATION: &str = "A new client id can take up to about 18 hours to reach athenahealth's \
     API gateway. Until then sign-in and consent work, and the token request answers 401 \
     InvalidClient (athena exits 5 and stores nothing): sign in again later.";

/// The registration for `env`, with this configuration's redirect ports.
pub fn form(config: &Config, env: RegisterEnv) -> Form {
    let env = Environment::from(env);
    let redirect_uris: Vec<String> = config
        .redirect_ports()
        .iter()
        .map(|port| format!("http://127.0.0.1:{port}/callback"))
        .collect();
    let configured = config.client_ids.get(env);
    let fields = vec![
        Field {
            field: "Environment",
            value: match env {
                Environment::Preview => "Preview".into(),
                _ => "Production".into(),
            },
            note: Some(match env {
                Environment::Preview => "athenahealth's sandbox and its synthetic patient",
                _ => "real practices; register Preview first to try the sandbox",
            }),
        },
        Field {
            field: "App name",
            value: "<your brand> Health Records".into(),
            note: Some(
                "your own brand plus what the app does. athenahealth refuses \"athena\", \
                 \"athenahealth\", \"app\", \"application\", \"PHR\", \"CAPI\", \"test\", \
                 environment words, people's names, and a name another app has: add a word",
            ),
        },
        Field {
            field: "Website",
            value: "https://github.com/piekstra/athena-cli".into(),
            note: Some("or the address of your own copy"),
        },
        Field {
            field: "Description",
            value: "Command-line tool with which a patient or caregiver downloads their own \
                    records from athenahealth practices to their own computer."
                .into(),
            note: None,
        },
        Field {
            field: "Intended end users",
            value: "The patient and approved family members or caregivers".into(),
            note: Some("a caregiver signs in for a child"),
        },
        Field {
            field: "OAuth client type",
            value: "Public (native)".into(),
            note: None,
        },
        Field {
            field: "Token authentication",
            value: "PKCE, no client secret".into(),
            note: None,
        },
        Field {
            field: "Redirect URLs",
            value: redirect_uris.join(" "),
            note: Some("each one exactly, port included"),
        },
    ];
    Form {
        schema: SCHEMA,
        vendor: "athenahealth",
        environment: env,
        portal: PORTAL_URL,
        doc: DOC_URL,
        app_type: [
            "My app will use Certified APIs ONLY",
            "3-Legged OAuth for Patients",
            "I confirm that I am building a PHR app",
        ],
        fields,
        launch: "standalone-patient",
        client_type: "public",
        client_secret: false,
        pkce: "S256",
        redirect_uris,
        scopes: Scopes {
            product: "FHIR R4 SMART V2",
            permissions: ["read (.r)", "search (.s)"],
            types: smart::PATIENT_TYPES.to_vec(),
            requested: smart::scopes(pk_med_smart::ScopeVersion::V2).to_string(),
        },
        store: store_command(env),
        configured: configured.is_some(),
        client_id_ends_with: configured.map(ends_with),
        propagation: PROPAGATION,
    }
}

impl Form {
    pub fn render(&self) {
        let env = &self.fields[0].value;
        println!("Registering an app for athena with athenahealth ({env})");
        println!();
        println!("Portal:  {}", self.portal);
        println!("Guide:   {}", self.doc);
        println!();
        println!("1. Choose (+) Create a New App, then:");
        for choice in self.app_type {
            println!("     {choice}");
        }
        println!("   and choose Continue to App Details.");
        println!("2. Fill in the form:");
        for field in &self.fields {
            let mut values = if field.field == "Redirect URLs" {
                self.redirect_uris.clone()
            } else {
                vec![field.value.clone()]
            };
            if let Some(note) = field.note {
                values.push(format!("({note})"));
            }
            let lines = values.iter().flat_map(|value| wrap(value, 50));
            for (i, line) in lines.enumerate() {
                let label = if i == 0 { field.field } else { "" };
                println!("     {label:<22}{line}");
            }
        }
        println!("3. Read the linked terms, tick the box, choose Create Application, and copy");
        println!("   the client id.");
        println!(
            "4. On the app's Scopes tab, open {}. For each type below,",
            self.scopes.product
        );
        println!(
            "   tick {} and {}, then choose Confirm Scopes:",
            self.scopes.permissions[0], self.scopes.permissions[1]
        );
        for line in wrap(&self.scopes.types.join(", "), 72) {
            println!("     {line}");
        }
        println!("5. Store the client id:");
        println!("     {}", self.store);
        match &self.client_id_ends_with {
            Some(tail) => println!("   Configured now: yes, ending {tail}."),
            None => println!("   Configured now: no."),
        }
        println!();
        for line in wrap(self.propagation, 78) {
            println!("{line}");
        }
    }
}

/// The last four characters of a client id: enough to tell two apart, while
/// `info` and `auth status` output, which gets pasted into bug reports,
/// carries no whole id. `config show` prints it whole.
pub fn ends_with(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// Which client ids are configured: each environment's, and each practice
/// profile that has its own. For `info` and `auth status`.
pub fn clients(config: &Config) -> Value {
    let env = |env: Environment| match config.client_ids.get(env) {
        Some(id) => json!({"configured": true, "client_id_ends_with": ends_with(id)}),
        None => json!({"configured": false, "register": command(env)}),
    };
    let practices: Vec<Value> = config
        .practices
        .iter()
        .filter_map(|(slug, practice)| {
            practice
                .client_id
                .as_deref()
                .map(|id| json!({"org": slug, "client_id_ends_with": ends_with(id)}))
        })
        .collect();
    let mut value = json!({
        "preview": env(Environment::Preview),
        "production": env(Environment::Production),
        "doc": DOC_URL,
    });
    if !practices.is_empty() {
        value["practices"] = Value::Array(practices);
    }
    value
}

/// One line for `auth status`: which environments have a client id.
pub fn clients_line(config: &Config) -> String {
    [Environment::Production, Environment::Preview]
        .iter()
        .map(|&env| match config.client_ids.get(env) {
            Some(id) => format!("{env} configured (ends {})", ends_with(id)),
            None => format!("{env} not configured ({})", command(env)),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
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
        let form = form(&config, RegisterEnv::Production);
        assert_eq!(
            form.redirect_uris,
            [
                "http://127.0.0.1:50123/callback",
                "http://127.0.0.1:50124/callback"
            ]
        );
        assert_eq!(
            form.store,
            "athena config set client-id <CLIENT_ID> --env production"
        );
        assert!(!form.configured);
    }

    #[test]
    fn the_form_asks_for_every_type_a_sign_in_requests() {
        let form = form(&Config::default(), RegisterEnv::Preview);
        for ty in smart::PATIENT_TYPES {
            assert!(
                form.scopes.requested.contains(&format!("patient/{ty}.rs")),
                "{ty}"
            );
        }
        assert_eq!(form.scopes.types.len(), smart::PATIENT_TYPES.len());
        assert_eq!(form.fields[0].value, "Preview");
    }

    #[test]
    fn a_client_id_is_shown_by_its_last_four_characters() {
        let mut config = Config::default();
        config
            .client_ids
            .set(Environment::Production, Some("0oaEXAMPLEclient9876".into()));
        let value = clients(&config);
        assert_eq!(value["production"]["configured"], true);
        assert_eq!(value["production"]["client_id_ends_with"], "9876");
        assert_eq!(value["preview"]["configured"], false);
        assert_eq!(
            value["preview"]["register"],
            "athena registration --env preview"
        );
        assert!(!value.to_string().contains("0oaEXAMPLE"));
        assert_eq!(ends_with("ab"), "ab");
    }

    #[test]
    fn wrapping_keeps_every_word() {
        let text = smart::PATIENT_TYPES.join(", ");
        let lines = wrap(&text, 30);
        assert!(lines.iter().all(|l| l.len() <= 30));
        assert_eq!(lines.join(" "), text);
    }
}
