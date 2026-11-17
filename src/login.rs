//! `auth login`: a SMART standalone patient launch in the system browser,
//! through medical-common's `pk_med_smart`.
//!
//! The browser opens athena's own start page, which carries athenahealth's
//! "Log in with athenahealth" button (athenahealth asks it of patient-facing
//! apps). The page is served on a loopback port of its own, because the
//! listener on the registered redirect port answers only `/callback`. The
//! button goes to athenahealth's authorize endpoint with `aud` set to the
//! practice's FHIR base, PKCE, state and nonce (`pk_med_smart::begin`). The
//! person signs in with their athenahealth patient account and picks one
//! patient record: their own, or a child's when they are its caregiver.
//! athenahealth's answer comes back to the listener, and
//! `SmartSignIn::exchange` trades the code for tokens and checks the nonce.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pk_cli_auth::oauth::{ClientAuth, OAuthClient};
use pk_cli_core::{block_on, CliError};
use pk_cli_http::BlockingTransport;
use pk_med_fhir::is_id;
#[cfg(test)]
use pk_med_fhir::MAX_ID_LEN;
use pk_med_smart::{begin, discover, LaunchRequest, SmartApp, SmartSession, SmartSignIn};

use crate::config::{Practice, ScopeSpelling};
use crate::env::Environment;
use crate::session::REVOCATION_ENDPOINT;
use crate::smart::{self, Athenahealth};
use crate::{BIN, VERSION};

/// athenahealth's assets for the button, from its "Log in with athenahealth"
/// guidelines.
const BUTTON_CSS: &str = "https://identity-static-assets.platform.athenahealth.com/login-with-athenahealth/latest/css/ah-login-button.min.css";
const BUTTON_LOGO: &str = "https://identity-static-assets.platform.athenahealth.com/login-with-athenahealth/latest/img/athenahealth_logo_white.svg";

/// What a sign-in needs from the command line and the config.
pub struct Request<'a> {
    /// The login profile signing in, which the remedies name.
    pub profile: Option<&'a str>,
    /// The practice profile's id: the session's organization.
    pub org: &'a str,
    pub practice: &'a Practice,
    pub org_name: &'a str,
    pub client_id: &'a str,
    pub ports: &'a [u16],
    pub timeout: Duration,
    /// The scope spelling to ask in (`athena config set scope-version`).
    pub scope_version: ScopeSpelling,
}

/// A finished sign-in, not yet stored.
pub struct Outcome {
    pub session: SmartSession,
    pub relationship: Option<&'static str>,
    /// The types athena asked to read that athenahealth did not grant: each
    /// export reports them as `forbidden` skips.
    pub not_granted: Vec<&'static str>,
}

/// What to do when athenahealth grants fewer read scopes than athena asked
/// for: the app's registration lacks them.
pub fn scope_remedy(profile: Option<&str>, org: &str, asked: ScopeSpelling) -> String {
    let other = match asked {
        ScopeSpelling::V2 => {
            "; an app that holds only FHIR R4 SMART V1 scopes signs in with `athena config set \
             scope-version v1`"
        }
        ScopeSpelling::V1 => "; or return to SMART v2 with `athena config unset scope-version`",
    };
    format!(
        "add FHIR R4 SMART V2 read and search for each type to the app's scopes in \
         athenahealth's Developer Portal (docs/registration.md, step 5){other}; then run \
         `{}` again",
        login_command(profile, org)
    )
}

/// The steps before the browser opens: discovery, the listener, the URL and
/// the start page.
pub struct Pending {
    profile: Option<String>,
    signin: SmartSignIn,
    start: StartPage,
    client: ClientAuth,
    org: String,
    revocation_endpoint: Option<String>,
    timeout: Duration,
    scope_version: ScopeSpelling,
}

/// Discover the server, bind the listener and serve the start page.
pub fn prepare(http: &BlockingTransport, request: &Request<'_>) -> Result<Pending, CliError> {
    let fhir_base = request.practice.fhir_base()?;
    let config = block_on(discover(http, &fhir_base, &Athenahealth, BIN, VERSION))?;
    let app = SmartApp::new(
        BIN,
        ClientAuth::public(request.client_id),
        request.ports.to_vec(),
    );
    let signin = begin(
        &app,
        LaunchRequest {
            config: &config,
            fhir_base: &fhir_base,
            aud: None,
            scopes: smart::scopes(request.scope_version.version()),
            quirks: &Athenahealth,
        },
    )?;
    let page = start_page(signin.url(), request.org_name, request.practice.environment);
    Ok(Pending {
        profile: request.profile.map(str::to_string),
        start: StartPage::serve(page)?,
        signin,
        client: ClientAuth::public(request.client_id),
        org: request.org.to_string(),
        revocation_endpoint: config.revocation_endpoint.clone(),
        timeout: request.timeout,
        scope_version: request.scope_version,
    })
}

/// Store `signed_in` under `lock`, replacing the profile's sign-in for the
/// same patient, whose tokens are then revoked so none is left live unseen.
/// `Some(revoked)` when a sign-in with other tokens was replaced.
pub fn store_replacing(
    http: &dyn crate::http::Http,
    lock: &pk_cli_auth::refresh_lock::RefreshLock,
    store: &dyn pk_med_smart::SessionStore,
    signed_in: &SmartSession,
) -> Result<Option<bool>, CliError> {
    // A previous sign-in that does not read is replaced without a revocation.
    let previous = store.load(&signed_in.account()).ok().flatten();
    lock.store_signin(|| store.save(signed_in))?;
    Ok(previous
        .filter(|old| {
            old.tokens.refresh_token != signed_in.tokens.refresh_token
                || old.tokens.access_token != signed_in.tokens.access_token
        })
        .map(|old| crate::logout::revoke_all(http, &old)))
}

/// `athena --profile <P> auth login --org <ORG>`, for a remedy.
fn login_command(profile: Option<&str>, org: &str) -> String {
    format!(
        "athena --profile {} auth login --org {org}",
        profile.unwrap_or("<NAME>")
    )
}

impl Pending {
    fn login_command(&self) -> String {
        login_command(self.profile.as_deref(), &self.org)
    }

    /// The page to open in the browser.
    pub fn start_url(&self) -> String {
        self.start.url.clone()
    }

    /// Wait for the callback, then trade the code for tokens.
    pub fn finish(self, http: &BlockingTransport, now: u64) -> Result<Outcome, CliError> {
        let again = self.login_command();
        let code = self
            .signin
            .wait_for_code(self.timeout, |refused| eprintln!("{refused}"))
            .map_err(|e| e.with_remedy(format!("start again with `{again}`")))?;
        drop(self.start);
        let oauth = OAuthClient::new(http.clone(), BIN, VERSION);
        let mut session =
            block_on(
                self.signin
                    .exchange(&oauth, &self.client, &code, &self.org, now),
            )
            .map_err(|e| {
                // pk_med_smart's remedies are generic ("sign in again").
                if e.is_auth() {
                    e.with_remedy(format!("start again with `{again}`"))
                } else {
                    e
                }
            })?;
        if let Some(endpoint) = self.revocation_endpoint {
            session
                .extra
                .insert(REVOCATION_ENDPOINT.into(), endpoint.into());
        }
        let Some(patient) = session.patient.clone() else {
            crate::logout::revoke_all(http, &session);
            return Err(CliError::Upstream(format!(
                "athenahealth's answer named no patient ({}), so the sign-in was not stored \
                 and its tokens were revoked",
                no_patient_facts(&session)
            ))
            .with_remedy(format!(
                "start again with `{again}`, and pick one record on athenahealth's \"Select a \
                 health record\" page"
            )));
        };
        validate_patient_id(&patient)?;
        let not_granted = smart::not_granted(session.tokens.scope.as_deref());
        if not_granted.len() == smart::PATIENT_TYPES.len() {
            // Nothing could be read: the sign-in is not stored, and its
            // tokens are revoked rather than left live and unknown.
            crate::logout::revoke_all(http, &session);
            return Err(CliError::Auth(format!(
                "athenahealth granted none of the {} read scopes athena asked for (SMART {}), \
                 so the sign-in was not stored",
                not_granted.len(),
                self.scope_version.as_str()
            ))
            .with_remedy(scope_remedy(
                self.profile.as_deref(),
                &self.org,
                self.scope_version,
            )));
        }
        let relationship = smart::relationship(session.fhir_user.as_deref(), &patient);
        Ok(Outcome {
            session,
            relationship,
            not_granted,
        })
    }
}

/// What tells apart the reasons a token answer names no patient, without an
/// id: whether `launch/patient` was granted, and the resource type of the
/// id token's `fhirUser`.
fn no_patient_facts(session: &SmartSession) -> String {
    let launch = session
        .tokens
        .scope
        .as_deref()
        .is_some_and(|s| s.split(' ').any(|item| item == "launch/patient"));
    let user = session
        .fhir_user
        .as_deref()
        .and_then(|u| u.rsplit('/').nth(1))
        .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or("none");
    format!(
        "launch/patient granted: {}; fhirUser: {user}",
        if launch { "yes" } else { "no" }
    )
}

/// athena's start page, on 127.0.0.1 at a port the system picks, served
/// from a thread until the sign-in is done, each connection on a short-lived
/// thread of its own (a browser's idle speculative connection then holds up
/// nothing). It answers `GET /` with a `Host` of its own (a page elsewhere
/// that rebinds a name to 127.0.0.1 sends its own), and 404 to anything else.
struct StartPage {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StartPage {
    fn serve(page: String) -> Result<StartPage, CliError> {
        let listen = |e: std::io::Error| {
            CliError::Other(format!(
                "could not listen on 127.0.0.1 for the sign-in's start page: {e}"
            ))
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(listen)?;
        let port = listener.local_addr().map_err(listen)?.port();
        listener.set_nonblocking(true).map_err(listen)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let page = Arc::new(page);
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let page = Arc::clone(&page);
                        thread::spawn(move || answer(stream, port, &page));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            }
        });
        Ok(StartPage {
            url: format!("http://127.0.0.1:{port}/"),
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for StartPage {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn answer(mut stream: TcpStream, port: u16, page: &str) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let get_root = request_line.next() == Some("GET") && request_line.next() == Some("/");
    let ours = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let host_is_ours = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("host"))
        .is_some_and(|(_, value)| ours.iter().any(|o| o.eq_ignore_ascii_case(value.trim())));
    let (status, reason, body) = if get_root && host_is_ours {
        (200, "OK", page)
    } else {
        (404, "Not Found", "nothing here")
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// A patient id is used in keychain accounts, file names and URLs, so it is
/// held to the id rule the reads apply (`pk_med_fhir::is_id`).
pub fn validate_patient_id(id: &str) -> Result<(), CliError> {
    let ok = is_id(id);
    if ok {
        Ok(())
    } else {
        Err(CliError::Upstream(
            "athenahealth's answer named a patient id that is not a FHIR id".into(),
        ))
    }
}

/// athena's start page: the button for athenahealth's environments, a plain
/// link otherwise.
pub fn start_page(authorize_url: &str, org_name: &str, environment: Environment) -> String {
    let href = html_escape(authorize_url);
    let org = html_escape(org_name);
    let (head, button) = if environment.is_athenahealth() {
        (
            format!("<link rel=\"stylesheet\" href=\"{BUTTON_CSS}\">"),
            format!(
                "<a id=\"signin\" href=\"{href}\" class=\"btn athenahealthsignin logo_white\" \
                 style=\"text-decoration:none\"><div class=\"ah_login_left_div\">Log in with</div>\
                 <div class=\"ah_login_right_div\"><img class=\"logo\" alt=\"athenahealth\" \
                 title=\"athenahealth\" src=\"{BUTTON_LOGO}\"></div></a>"
            ),
        )
    } else {
        (
            String::new(),
            format!("<a id=\"signin\" href=\"{href}\">Sign in</a>"),
        )
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>athena: sign in</title>{head}\
         <meta name=\"referrer\" content=\"no-referrer\"><style>body{{font-family:system-ui,\
         sans-serif;max-width:34rem;margin:4rem auto;padding:0 1rem;line-height:1.5}}</style>\
         </head><body><h1>Sign in to {org}</h1><p>Sign in with the patient portal account. If \
         you are a caregiver, pick the one patient record this sign-in is for; sign in again for \
         each other one.</p><p>{button}</p></body></html>"
    )
}

/// Escape text for HTML content and attribute values.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_page_carries_the_button_and_an_escaped_url() {
        let page = start_page(
            "https://api.preview.platform.athenahealth.com/oauth2/v1/authorize?a=1&b=<x>",
            "Clinic <One>",
            Environment::Preview,
        );
        assert!(page.contains("athenahealthsignin"));
        assert!(page.contains("a=1&amp;b=&lt;x&gt;"));
        assert!(page.contains("Clinic &lt;One&gt;"));
        let plain = start_page("http://127.0.0.1:1/authorize", "Mock", Environment::Custom);
        assert!(!plain.contains("identity-static-assets"));
    }

    fn get(port: u16, host: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    }

    #[test]
    fn the_start_page_answers_its_own_host_at_the_root_only() {
        let start = StartPage::serve("<p>the page</p>".into()).unwrap();
        let port: u16 = start
            .url
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let own = get(port, &format!("127.0.0.1:{port}"), "/");
        assert!(own.starts_with("HTTP/1.1 200"), "{own}");
        assert!(own.contains("<p>the page</p>"));
        assert!(own.contains("Cache-Control: no-store"));
        // An idle connection, as a browser opens speculatively, holds up
        // nothing.
        let _idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let started = std::time::Instant::now();
        let again = get(port, &format!("localhost:{port}"), "/");
        assert!(again.starts_with("HTTP/1.1 200"), "{again}");
        assert!(started.elapsed() < Duration::from_secs(1));
        let rebound = get(port, &format!("rebound.example.test:{port}"), "/");
        assert!(rebound.starts_with("HTTP/1.1 404"), "{rebound}");
        let elsewhere = get(port, &format!("127.0.0.1:{port}"), "/favicon.ico");
        assert!(elsewhere.starts_with("HTTP/1.1 404"), "{elsewhere}");
        drop(start);
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    #[test]
    fn patient_ids_are_fhir_ids() {
        assert!(validate_patient_id("a-195900.E-60178").is_ok());
        assert!(validate_patient_id(&"a".repeat(MAX_ID_LEN)).is_ok());
        assert!(validate_patient_id(&"a".repeat(MAX_ID_LEN + 1)).is_err());
        assert!(validate_patient_id("../x").is_err());
        assert!(validate_patient_id("").is_err());
        assert!(validate_patient_id("...").is_err());
    }
}
