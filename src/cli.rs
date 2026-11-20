//! The command line: the clap tree and one handler per command.

use std::cell::OnceCell;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use pk_cli_auth::refresh_lock::RefreshLock;
use pk_cli_auth::vault::{KeyStore, Migration, Vault};
use pk_cli_auth::{AuthLogin, AuthMethod, AuthStatus};
use pk_cli_config::profile::{self, ProfileName};
use pk_cli_config::ConfigStore;
use pk_cli_core::info::{AuthInfo, CliInfo};
use pk_cli_core::{confirm, dates, output, CliError, CommonArgs, Paged, RangeArgs, Reason};
use pk_cli_documents::{DownloadBatch, SavedDocument, SkippedDocument};
use pk_cli_http::BlockingTransport;
use pk_cli_selfupdate::{SelfUpdateArgs, Updater};
use serde_json::{json, Map, Value};

use crate::config::{self, Config, Practice};
use crate::directory::{self, Directory};
use crate::documents;
use crate::env::Environment;
use crate::export;
use crate::fhir::{Client, Live};
use crate::login;
use crate::logout;
use crate::registration::{self, RegisterEnv};
use crate::session::{self, SessionEntry, SessionIndex, Store, PROFILE_ENV};
use crate::stream::{self, Header};
use crate::{unix_now, BIN, REPO, VERSION};
use pk_med_smart::{migrate_sessions, ProfileSessions, SessionStore, SignInWindow, SmartSession};

/// Unofficial; not affiliated with or endorsed by athenahealth. A patient's or
/// caregiver's own records from athenahealth practices, through athenahealth's
/// patient-access SMART on FHIR API. Read-only: it changes nothing at any
/// practice.
#[derive(Parser, Debug)]
#[command(name = BIN, version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub common: CommonArgs,
    /// The config file (default: ~/.config/athena/config.json). Session and
    /// directory state live beside it.
    #[arg(long, global = true, env = "ATHENA_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// The login profile to run as, one per person who signs in (`athena
    /// profiles list`); ATHENA_PROFILE when absent. Every command that uses a
    /// sign-in (auth, patients, records, documents, api) needs one, with no
    /// default; the others refuse it.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in, see who is signed in, sign out.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Login profiles, one per person who signs in (profile-list/v1).
    #[command(subcommand)]
    Profiles(ProfilesCmd),
    /// Non-secret settings: client ids, redirect ports, practice profiles.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Practice profiles, and athenahealth's directory of practices (org-list/v1).
    #[command(subcommand)]
    Orgs(OrgsCmd),
    /// The patients signed in (patient-list/v1).
    #[command(subcommand)]
    Patients(PatientsCmd),
    /// Everything a sign-in can read, as a records-stream/v1 stream.
    #[command(subcommand)]
    Records(RecordsCmd),
    /// Files: documents and report forms, fetched as Binary (documents/v1).
    #[command(subcommand)]
    Documents(DocumentsCmd),
    /// A raw read from the signed-in FHIR server: `athena api GET Patient/<id>`.
    Api(ApiCmd),
    /// Update to the latest release from GitHub.
    SelfUpdate(SelfUpdateArgs),
    /// Print a shell completion script.
    Completions { shell: Shell },
    /// What to enter in athenahealth's Developer Portal to register an app
    /// for athena (registration/v1).
    Registration(RegistrationArgs),
    /// Machine-readable capability discovery (cli-info/v1).
    Info,
}

#[derive(Args, Debug)]
pub struct RegistrationArgs {
    /// The environment to register in: production for real practices,
    /// preview for athenahealth's sandbox.
    #[arg(long, value_enum, default_value_t = RegisterEnv::Production)]
    pub env: RegisterEnv,
}

/// Which signed-in patient a command reads.
#[derive(Args, Debug, Clone, Default)]
pub struct Select {
    /// The patient: its FHIR id or the label given at sign-in. Optional when
    /// the profile has only one sign-in.
    #[arg(long, value_name = "ID|LABEL")]
    pub patient: Option<String>,
    /// The practice profile, when the patient is signed in at more than one.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ProfilesCmd {
    /// The configured profiles (profile-list/v1), and any sign-in stored
    /// before v0.4.0 that is in none.
    #[command(visible_alias = "ls")]
    List,
    /// Add a profile: one per person who signs in (`adult1`).
    Add {
        /// 1 to 32 lowercase letters, digits, `-` or `_`.
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Sign out a profile's sign-ins, revoking their tokens, and remove it
    /// with its browser profile (profile-remove/v1).
    Remove {
        /// The profile to remove (not `--profile`).
        #[arg(value_name = "NAME")]
        name: String,
        /// Do not ask first.
        #[arg(long)]
        force: bool,
    },
    /// Move sign-ins stored before v0.4.0 into a profile (profile-migrate/v1).
    /// No sign-in, and no request unless it replaces a sign-in the profile
    /// already holds, whose own tokens are then revoked; a newly built athena
    /// asks the Keychain once for each sign-in it moves.
    Migrate(MigrateCmd),
}

#[derive(Args, Debug)]
pub struct MigrateCmd {
    /// The profile to move them into (not `--profile`); added when it is new.
    #[arg(value_name = "NAME")]
    pub name: String,
    /// The practice profile the sign-ins belong to.
    #[arg(long, value_name = "ORG")]
    pub org: String,
    /// A sign-in to move, by patient id or label (`athena profiles list`
    /// shows them); repeat for each.
    #[arg(long = "patient", value_name = "ID|LABEL", required = true)]
    pub patients: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Sign in in the browser and pick one patient record (auth-login/v1).
    Login(LoginCmd),
    /// Who is signed in, and until when (auth-status/v1). Reads the keychain,
    /// never the network.
    Status(Select),
    /// Revoke and forget a sign-in.
    Logout(LogoutCmd),
    /// Rename a sign-in's label (auth-relabel/v1). The label lives only in
    /// athena's local index: no keychain, no network.
    Relabel(RelabelCmd),
}

#[derive(Args, Debug)]
pub struct RelabelCmd {
    /// The practice profile the sign-in belongs to.
    #[arg(long, value_name = "ORG")]
    pub org: String,
    /// The label the sign-in has now.
    #[arg(long, value_name = "LABEL")]
    pub from: String,
    /// The label to give it.
    #[arg(long, value_name = "LABEL")]
    pub to: String,
}

#[derive(Args, Debug)]
pub struct LoginCmd {
    /// The practice profile to sign in to (`athena orgs list`).
    #[arg(long, value_name = "ORG")]
    pub org: String,
    /// A short name for the patient this sign-in picks (`kid1`, `me`), so
    /// later commands can say `--patient <LABEL>`.
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,
    /// Print the sign-in page's address instead of opening the profile's
    /// browser window.
    #[arg(long)]
    pub no_browser: bool,
    /// Minutes to wait for the sign-in.
    #[arg(long, value_name = "MINUTES", default_value_t = 30,
          value_parser = clap::value_parser!(u64).range(1..=60))]
    pub timeout: u64,
}

#[derive(Args, Debug)]
pub struct LogoutCmd {
    #[command(flatten)]
    pub select: Select,
    /// Every signed-in patient (within --org, if given).
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print the config file's path.
    Path,
    /// Show the configuration.
    Show,
    /// Set a value.
    #[command(subcommand)]
    Set(SetCmd),
    /// Remove a value.
    #[command(subcommand)]
    Unset(UnsetCmd),
}

#[derive(Subcommand, Debug)]
pub enum SetCmd {
    /// The app's client id from athenahealth's Developer Portal, per environment.
    ClientId {
        #[arg(value_name = "CLIENT_ID")]
        id: String,
        #[arg(long, value_enum)]
        env: Environment,
    },
    /// The loopback ports registered as redirect URIs, tried in order (default 47816,47817).
    RedirectPorts {
        #[arg(value_name = "PORTS")]
        ports: String,
    },
    /// The SMART scope spelling sign-ins ask in: v2 (default, patient/<Type>.rs), or v1
    /// (patient/<Type>.read) for an app registered with SMART V1 scopes only.
    ScopeVersion {
        #[arg(value_enum, value_name = "VERSION")]
        version: config::ScopeSpelling,
    },
    /// Add or change a practice profile.
    Practice(PracticeArgs),
}

#[derive(Args, Debug)]
pub struct PracticeArgs {
    /// The profile's name: 1 to 40 of a-z, 0-9 and -.
    #[arg(value_name = "SLUG")]
    pub slug: String,
    /// A display name.
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_enum)]
    pub env: Option<Environment>,
    /// athenaOne practice id (`orgs search` shows it).
    #[arg(long, value_name = "ID")]
    pub practice_id: Option<String>,
    /// Patient-portal brand id.
    #[arg(long, value_name = "ID")]
    pub brand_id: Option<String>,
    /// Chart sharing group id.
    #[arg(long, value_name = "ID")]
    pub csg_id: Option<String>,
    /// The practice's patient portal host, for reference.
    #[arg(long, value_name = "HOST")]
    pub portal_host: Option<String>,
    /// A FHIR base URL in full (required for --env custom).
    #[arg(long, value_name = "URL")]
    pub fhir_base: Option<String>,
    /// A client id for this profile alone.
    #[arg(long, value_name = "CLIENT_ID")]
    pub client_id: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum UnsetCmd {
    ClientId {
        #[arg(long, value_enum)]
        env: Environment,
    },
    RedirectPorts,
    ScopeVersion,
    Practice {
        #[arg(value_name = "SLUG")]
        slug: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum OrgsCmd {
    /// The practice profiles `--org` can name.
    #[command(visible_alias = "ls")]
    List,
    /// Search the cached directory of athenahealth practices by name, city, state, ZIP or id.
    ///
    /// Each match's `id` is the directory's `<practice>-<brand>-<csg>` key,
    /// not a profile `--org` can name: save one with `config set practice
    /// <SLUG> --practice-id <P> --brand-id <B> --csg-id <C>`.
    Search {
        /// Words that must all appear in the name, city, state, ZIP or ids
        #[arg(value_name = "TEXT", required = true)]
        text: Vec<String>,
        /// The most matches to show
        #[arg(long, value_name = "N", default_value_t = 25)]
        limit: usize,
    },
    /// Download athenahealth's practice directory (about 135 MB) into the local cache.
    Refresh {
        #[arg(long, hide = true, default_value = directory::DIRECTORY_URL)]
        url: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum PatientsCmd {
    /// The signed-in patients, from the local index (no keychain, no network).
    #[command(visible_alias = "ls")]
    List {
        /// Only the patients signed in at this practice profile
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum RecordsCmd {
    /// Write every resource the sign-in can read as records-stream/v1.
    Export(ExportCmd),
}

#[derive(Args, Debug)]
pub struct ExportCmd {
    #[command(flatten)]
    pub select: Select,
    /// Only resources updated on or after this day.
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub since: Option<String>,
    /// Write the stream to stdout, for `medrec import --from athena`.
    #[arg(long, conflicts_with = "out")]
    pub ndjson: bool,
    /// Write the stream to DIR/records.ndjson and print records-export/v1:
    /// the sandbox and test servers only. A run cut short writes
    /// DIR/records.incomplete.ndjson and keeps the last complete export.
    #[arg(short = 'o', long = "out", value_name = "DIR")]
    pub out: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum DocumentsCmd {
    /// The patient's documents, newest first (document-list/v1).
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        select: Select,
        #[command(flatten)]
        range: RangeArgs,
    },
    /// Save one document (document-download/v1), or all of them with --all
    /// (document-download-batch/v1).
    Download {
        /// The id `documents list` shows.
        #[arg(
            value_name = "ID",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        id: Option<String>,
        /// Every document, into the directory -o names.
        #[arg(long)]
        all: bool,
        /// Where to write: a file (its directory must exist), or a directory to
        /// write into; end it with / to have athena create the directory.
        #[arg(short = 'o', long = "out", value_name = "PATH")]
        out: PathBuf,
        #[command(flatten)]
        select: Select,
        #[command(flatten)]
        range: RangeArgs,
    },
}

#[derive(Args, Debug)]
pub struct ApiCmd {
    /// GET only: athena is read-only.
    #[arg(value_name = "METHOD")]
    pub method: String,
    /// A path under the FHIR base (`Patient/<id>`), or a URL on its host.
    #[arg(value_name = "PATH")]
    pub path: String,
    #[command(flatten)]
    pub select: Select,
}

/// The lock beside the session index that every write of it holds.
const INDEX_LOCK: &str = ".sessions.lock";

/// Paths, output mode and the profile flag, shared by every handler.
struct Ctx {
    json: bool,
    quiet: bool,
    interactive: bool,
    store: ConfigStore,
    /// `--profile`, unchecked.
    profile: Option<String>,
    /// One vault per process, so its key is read from the keychain once.
    vault: OnceCell<Arc<Vault>>,
}

impl Ctx {
    /// The login profile this command runs as: `--profile`, else
    /// ATHENA_PROFILE, and configured. Exit 2 otherwise, before any keychain
    /// read or request.
    fn profile(&self, config: &Config) -> Result<ProfileName, CliError> {
        profile::resolve(self.profile.as_deref(), PROFILE_ENV, &config.profiles, BIN)
    }

    fn vault(&self) -> Result<Arc<Vault>, CliError> {
        if let Some(vault) = self.vault.get() {
            return Ok(vault.clone());
        }
        let vault = session::vault(&self.state_dir()?)?;
        Ok(self.vault.get_or_init(|| vault).clone())
    }

    /// `profile`'s sign-ins in the vault.
    fn sessions(&self, profile: &ProfileName) -> Result<Store, CliError> {
        Ok(session::profile_store(self.vault()?, profile.clone()))
    }

    /// Change the session index as it is on disk now: read, change and
    /// write it under a lock of its own, so a sign-in, relabel or sign-out in
    /// another process meanwhile stands. Nothing is written when `change`
    /// fails.
    fn update_index<R>(
        &self,
        change: impl FnOnce(&mut SessionIndex) -> Result<R, CliError>,
    ) -> Result<R, CliError> {
        let dir = self.state_dir()?;
        RefreshLock::at(dir.join(INDEX_LOCK)).hold(|| {
            let mut index = SessionIndex::load_in(&dir)?;
            let result = change(&mut index)?;
            index.save(&dir.join(session::INDEX_FILE))?;
            Ok(result)
        })
    }

    fn config_path(&self) -> Result<PathBuf, CliError> {
        self.store.path()
    }

    fn state_dir(&self) -> Result<PathBuf, CliError> {
        let path = self.config_path()?;
        Ok(path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")))
    }

    #[cfg(test)]
    fn sessions_path(&self) -> Result<PathBuf, CliError> {
        Ok(self.state_dir()?.join(session::INDEX_FILE))
    }

    fn directory_path(&self) -> Result<PathBuf, CliError> {
        Ok(self.state_dir()?.join("directory.json"))
    }

    fn load(&self) -> Result<Config, CliError> {
        self.store.load()
    }

    /// Save the config readable by the owner alone: it names the household's
    /// practices.
    fn save(&self, config: &Config) -> Result<(), CliError> {
        let text = serde_json::to_string_pretty(config)
            .map_err(|e| CliError::Other(format!("encoding the config: {e}")))?;
        session::write_private(&self.config_path()?, text.as_bytes())
    }

    fn index(&self) -> Result<SessionIndex, CliError> {
        SessionIndex::load_in(&self.state_dir()?)
    }

    fn note(&self, message: impl AsRef<str>) {
        if !self.quiet {
            eprintln!("{}", message.as_ref());
        }
    }
}

/// Parse, run, and exit with the contract's code.
pub fn main() {
    let cli = Cli::parse();
    let json = cli.common.json;
    match run(cli) {
        Ok(()) => {}
        Err(e) => std::process::exit(output::fail(&e, json)),
    }
}

pub fn run(cli: Cli) -> Result<(), CliError> {
    if let Some(path) = &cli.config {
        if path.as_os_str().is_empty() {
            return Err(CliError::Usage("--config needs a path".into()));
        }
    }
    let ctx = Ctx {
        json: cli.common.json,
        quiet: cli.common.quiet,
        interactive: cli.common.interactive(),
        store: ConfigStore::new(BIN).with_override(cli.config.clone()),
        profile: cli.profile.clone(),
        vault: OnceCell::new(),
    };
    let takes_profile = matches!(
        cli.command,
        Command::Auth(_)
            | Command::Patients(_)
            | Command::Records(_)
            | Command::Documents(_)
            | Command::Api(_)
    );
    if cli.profile.is_some() && !takes_profile {
        return Err(CliError::Usage(
            "--profile is for the commands that use a sign-in (auth, patients, records, \
             documents, api); `profiles remove` and `profiles migrate` take the profile as \
             their NAME"
                .into(),
        ));
    }
    match cli.command {
        Command::Auth(AuthCmd::Login(args)) => auth_login(&ctx, args),
        Command::Auth(AuthCmd::Status(select)) => auth_status(&ctx, &select),
        Command::Auth(AuthCmd::Logout(args)) => auth_logout(&ctx, args),
        Command::Auth(AuthCmd::Relabel(args)) => auth_relabel(&ctx, args),
        Command::Profiles(cmd) => profiles_cmd(&ctx, cmd),
        Command::Config(cmd) => config_cmd(&ctx, cmd),
        Command::Orgs(cmd) => orgs(&ctx, cmd),
        Command::Patients(PatientsCmd::List { org }) => patients_list(&ctx, org.as_deref()),
        Command::Records(RecordsCmd::Export(args)) => records_export(&ctx, args),
        Command::Documents(cmd) => documents_cmd(&ctx, cmd),
        Command::Api(args) => api(&ctx, args),
        Command::SelfUpdate(args) => Updater {
            repo: REPO.into(),
            binary: BIN.into(),
            target: env!("BUILD_TARGET").into(),
            current: VERSION.into(),
        }
        .run(&args, ctx.json, ctx.quiet),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), BIN, &mut std::io::stdout());
            Ok(())
        }
        Command::Registration(args) => registration_cmd(&ctx, &args),
        Command::Info => info(&ctx),
    }
}

fn registration_cmd(ctx: &Ctx, args: &RegistrationArgs) -> Result<(), CliError> {
    // The command that explains setup works even when the configuration
    // does not: it falls back to the default redirect ports, and says so.
    let config = ctx.load().unwrap_or_else(|e| {
        eprintln!(
            "warning: the configuration could not be read ({}); showing the default redirect ports",
            e.message()
        );
        Config::default()
    });
    let form = registration::form(&config, args.env.into());
    if ctx.json {
        output::json(&serde_json::to_value(&form).unwrap_or(Value::Null));
    } else {
        print!("{}", form.text());
    }
    Ok(())
}

fn info(ctx: &Ctx) -> Result<(), CliError> {
    let info = CliInfo::new(
        BIN,
        VERSION,
        &format!("https://github.com/{REPO}"),
        AuthInfo {
            required: true,
            method: "browser-session".into(),
            login_hint: Some(
                "athena --profile <NAME> auth login --org <ORG> [--label <LABEL>]".into(),
            ),
        },
        &[
            "orgs",
            "profiles",
            "auth",
            "patients",
            "records",
            "documents",
            "api",
            pk_med_records::LOGIN_PROFILES,
            pk_med_records::REGISTRATION,
        ],
    )
    .with_profiles(&["health-records/v1", pk_cli_documents::PROFILE]);
    // A configuration that does not parse leaves `client` out, with a
    // warning, rather than failing discovery or guessing. `with_client`
    // fails only when cli-common's DTO grows a `client` of its own
    // (medical-common DESIGN §3.8), which a test then catches.
    let value = match ctx.load() {
        Ok(config) => registration::with_client(&info, &config)?,
        Err(e) => {
            eprintln!(
                "warning: the configuration could not be read ({}); `client` is left out",
                e.message()
            );
            serde_json::to_value(&info).unwrap_or(Value::Null)
        }
    };
    // Always JSON: `info` is for machines (SPEC v1 §1.6).
    output::json(&value);
    Ok(())
}

// ---------------------------------------------------------------- auth

fn validate_label(flag: &str, label: &str) -> Result<(), CliError> {
    let ok = !label.is_empty()
        && label.len() <= 40
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(CliError::Usage(format!(
            "{flag} takes 1 to 40 of letters, digits, - and _"
        )))
    }
}

fn auth_login(ctx: &Ctx, args: LoginCmd) -> Result<(), CliError> {
    config::validate_slug(&args.org)?;
    if let Some(label) = &args.label {
        validate_label("--label", label)?;
    }
    let config = ctx.load()?;
    let profile = ctx.profile(&config)?;
    let practice = config.practice(&args.org)?;
    let client_id = config.client_id_for(&practice)?;
    let ports = config.redirect_ports();

    let http = BlockingTransport::new()?;
    let lock = RefreshLock::for_binary(BIN)?;
    lock.before_signin()?;
    let org_name = practice.display_name(&args.org);
    let pending = login::prepare(
        &http,
        &login::Request {
            profile: profile.as_str(),
            org: &args.org,
            practice: &practice,
            org_name: &org_name,
            client_id: &client_id,
            ports: &ports,
            timeout: Duration::from_secs(args.timeout * 60),
            scope_version: config.scope_version(),
        },
    )?;
    let url = pending.start_url();
    let window = if args.no_browser {
        eprintln!("Open {url} in a browser to sign in to {org_name} as profile {profile}.");
        None
    } else {
        ctx.note(format!(
            "Opening profile {profile}'s browser window to sign in to {org_name}."
        ));
        match SignInWindow::open_at(BIN, &profile, &url, &[]) {
            Ok(window) => Some(window),
            Err(e) => {
                eprintln!(
                    "{e}; open {url} in a browser to sign in. A browser where another \
                     profile's login is signed in to athenahealth signs in as that login: \
                     use a private window."
                );
                None
            }
        }
    };
    let finished = pending.finish(&http, unix_secs());
    // Chrome quits gracefully, so the profile keeps the cookies it received.
    drop(window);
    let outcome = finished?;
    let not_granted = outcome.not_granted;
    if !not_granted.is_empty() {
        eprintln!(
            "warning: athenahealth did not grant reading {}; each export reports them as \
             forbidden skips. To fix it, {}",
            not_granted.join(", "),
            login::scope_remedy(profile.as_str(), &args.org, config.scope_version())
        );
    }
    let signed_in = outcome.session;
    let patient = signed_in.patient.clone().unwrap_or_default();
    let store = ctx.sessions(&profile)?;
    let replaced_revoked = login::store_replacing(&http, &lock, &*store, &signed_in)?;

    let mut entry = SessionEntry {
        profile: Some(profile.to_string()),
        org: args.org.clone(),
        patient: patient.clone(),
        label: args.label.clone(),
        relationship: outcome.relationship.map(str::to_string),
        environment: practice.environment,
        fhir_base: signed_in.fhir_base.clone(),
        stored_at: dates::fmt_rfc3339(unix_now()),
        signin_expires_at: Some(rfc3339(session::signin_expires_at(&signed_in))),
        login: session::login_fingerprint(&signed_in),
    };
    let login_matches = ctx.update_index(|index| {
        let matches = login_matches_profile(index, &entry);
        // A sign-in that keeps the label of another patient at this practice
        // in this profile takes it over.
        if let Some(label) = &entry.label {
            for other in index.sessions.iter_mut() {
                if other.profile == entry.profile
                    && other.org == entry.org
                    && other.patient != entry.patient
                    && other.label.as_deref() == Some(label)
                {
                    other.label = None;
                }
            }
        }
        index.upsert(entry.clone());
        Ok(matches)
    })?;
    if login_matches == Some(false) {
        eprintln!(
            "warning: this sign-in came from another athenahealth login than profile \
             {profile}'s other sign-ins. If it is another person's, sign it out with `athena \
             --profile {profile} auth logout --org {org} --patient {patient}` and sign in in \
             their profile's window (`athena --profile <NAME> auth login --org {org}`).",
            org = entry.org
        );
    }

    // Prove the session with one read; the record itself is discarded.
    let live = Live::start_in(
        Some(profile.as_str()),
        store,
        lock,
        http.clone(),
        signed_in.clone(),
    )?;
    let client = Client::new(http, live)?;
    let verified = match client.read("Patient", &patient) {
        Ok(_) => true,
        Err(e) => {
            eprintln!(
                "warning: the sign-in is stored, but reading the patient failed ({}): {}",
                e.code(),
                e.message()
            );
            false
        }
    };
    let after = client.session();
    if renewed(&signed_in, &after) {
        entry.signin_expires_at = Some(rfc3339(session::signin_expires_at(&after)));
        note_renewal(ctx, &entry, &signed_in, &after);
    }

    let mut value = AuthLogin::stored(verified).to_json();
    if let Value::Object(map) = &mut value {
        map.insert("profile".into(), json!(profile.as_str()));
        map.insert("org".into(), json!(entry.org));
        map.insert("patient".into(), json!(entry.patient));
        if let Some(label) = &entry.label {
            map.insert("label".into(), json!(label));
        }
        if let Some(rel) = &entry.relationship {
            map.insert("relationship".into(), json!(rel));
        }
        map.insert("signin_expires_at".into(), json!(entry.signin_expires_at));
        map.insert("types_not_granted".into(), json!(not_granted));
        if let Some(matches) = login_matches {
            map.insert("login_matches_profile".into(), json!(matches));
        }
        if let Some(revoked) = replaced_revoked {
            map.insert("replaced".into(), json!({ "revoked": revoked }));
        }
    }
    if ctx.json {
        output::json(&value);
    } else {
        println!(
            "Signed in to {org_name} as profile {profile}: patient {}{}{}.",
            entry.patient,
            entry
                .label
                .as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default(),
            match entry.relationship.as_deref() {
                Some("proxy") => ", as caregiver",
                _ => "",
            }
        );
        println!("Verified: {}", if verified { "yes" } else { "no" });
        if let Some(revoked) = replaced_revoked {
            println!(
                "Replaced: an earlier sign-in for this patient{}",
                if revoked {
                    ", its tokens revoked"
                } else {
                    "; its tokens could not be revoked and lapse on their own"
                }
            );
        }
        if let Some(at) = &entry.signin_expires_at {
            println!("Sign in again by: {at}");
        }
    }
    Ok(())
}

/// Whether `entry`'s login is the one `index`'s other sign-ins in its
/// profile came from, in the same environment (an issuer of its own). `None`
/// when there is nothing to compare: no fingerprint, or no other sign-in
/// with one.
fn login_matches_profile(index: &SessionIndex, entry: &SessionEntry) -> Option<bool> {
    let login = entry.login.as_deref()?;
    let others: Vec<&str> = index
        .matching(entry.profile.as_deref(), None, None)
        .into_iter()
        .filter(|e| e.environment == entry.environment)
        .filter(|e| !(e.org == entry.org && e.patient == entry.patient))
        .filter_map(|e| e.login.as_deref())
        .collect();
    if others.is_empty() {
        None
    } else {
        Some(others.iter().all(|other| *other == login))
    }
}

fn auth_status(ctx: &Ctx, select: &Select) -> Result<(), CliError> {
    if let Some(org) = &select.org {
        config::validate_slug(org)?;
    }
    let config = ctx.load()?;
    let profile = ctx.profile(&config)?;
    let index = ctx.index()?;
    let entries = index.matching(
        Some(profile.as_str()),
        select.patient.as_deref(),
        select.org.as_deref(),
    );
    let now = unix_secs();
    let store = ctx.sessions(&profile)?;
    let mut sessions = Vec::new();
    let mut any_usable = false;
    for entry in entries {
        let mut row = Map::new();
        row.insert("org".into(), json!(entry.org));
        row.insert("patient".into(), json!(entry.patient));
        if let Some(label) = &entry.label {
            row.insert("label".into(), json!(label));
        }
        if let Some(rel) = &entry.relationship {
            row.insert("relationship".into(), json!(rel));
        }
        row.insert("environment".into(), json!(entry.environment));
        if let Some(login) = &entry.login {
            row.insert("login".into(), json!(login));
        }
        match store.load(&entry.key()) {
            Ok(Some(stored)) => {
                any_usable |= stored_columns(&mut row, &stored, now);
            }
            Ok(None) => {
                row.insert("credential_in_keychain".into(), json!(false));
                row.insert("usable".into(), json!(false));
            }
            Err(e) => {
                row.insert("usable".into(), json!(false));
                row.insert("error".into(), json!(e.code()));
                eprintln!(
                    "warning: reading the sign-in of {} at {} failed: {}",
                    entry.patient,
                    entry.org,
                    e.message()
                );
            }
        }
        sessions.push(Value::Object(row));
    }
    let mut status = AuthStatus::new(true, any_usable, AuthMethod::BrowserSession);
    status.credential_in_keychain =
        Some(sessions.iter().any(|s| s["credential_in_keychain"] == true));
    status.session_valid = Some(any_usable);
    let unassigned = index.unassigned();
    if unassigned > 0 {
        ctx.note(unassigned_note(unassigned));
    }
    if ctx.json {
        let mut value = registration::with_client(&status.to_json(), &config)?;
        if let Value::Object(map) = &mut value {
            map.insert("profile".into(), json!(profile.as_str()));
            map.insert("sessions".into(), Value::Array(sessions));
            if unassigned > 0 {
                map.insert("sign_ins_in_no_profile".into(), json!(unassigned));
            }
        }
        output::json(&value);
    } else {
        status.render();
        println!("Profile:       {profile}");
        println!("Client:        {}", registration::status_line(&config));
        if sessions.is_empty() {
            println!(
                "Patients:      none signed in (athena --profile {profile} auth login --org <ORG>)"
            );
        } else {
            println!();
            output::table(&output::table_view(
                &sessions,
                &[
                    "org",
                    "patient",
                    "label",
                    "relationship",
                    "usable",
                    "signin_expires_at",
                ],
            ));
        }
    }
    Ok(())
}

/// The columns `auth status` shows for a stored sign-in, and whether it is
/// usable. Its client id appears by its last four characters only.
fn stored_columns(row: &mut Map<String, Value>, stored: &SmartSession, now: u64) -> bool {
    let usable = session::usable(stored, now);
    row.insert("credential_in_keychain".into(), json!(true));
    row.insert("usable".into(), json!(usable));
    row.insert(
        "client_id_ends_with".into(),
        json!(registration::ends_with(&stored.client_id)),
    );
    row.insert(
        "refresh_token".into(),
        json!(stored.tokens.refresh_token.is_some()),
    );
    row.insert(
        "access_expires_at".into(),
        json!(stored.tokens.expires_at.map(rfc3339)),
    );
    row.insert(
        "signin_expires_at".into(),
        json!(rfc3339(session::signin_expires_at(stored))),
    );
    if let Some(at) = stored.refused_at {
        row.insert("refused_at".into(), json!(rfc3339(at)));
    }
    usable
}

fn auth_relabel(ctx: &Ctx, args: RelabelCmd) -> Result<(), CliError> {
    config::validate_slug(&args.org)?;
    validate_label("--from", &args.from)?;
    validate_label("--to", &args.to)?;
    let profile = ctx.profile(&ctx.load()?)?;
    let entry =
        ctx.update_index(|index| index.relabel(profile.as_str(), &args.org, &args.from, &args.to))?;
    let value = json!({
        "schema": "auth-relabel/v1",
        "profile": profile.as_str(),
        "org": entry.org,
        "patient": entry.patient,
        "from": args.from,
        "to": args.to,
    });
    if ctx.json {
        output::json(&value);
    } else {
        println!(
            "Relabelled {} at {}: {} is now {}.",
            entry.patient, entry.org, args.from, args.to
        );
    }
    Ok(())
}

fn auth_logout(ctx: &Ctx, args: LogoutCmd) -> Result<(), CliError> {
    if let Some(org) = &args.select.org {
        config::validate_slug(org)?;
    }
    let profile = ctx.profile(&ctx.load()?)?;
    let chosen: Vec<SessionEntry> = ctx
        .index()?
        .matching(
            Some(profile.as_str()),
            args.select.patient.as_deref(),
            args.select.org.as_deref(),
        )
        .into_iter()
        .cloned()
        .collect();
    if chosen.is_empty() {
        return Err(CliError::NotFound(format!(
            "no signed-in patient in profile `{profile}` matches"
        ))
        .with_remedy(format!(
            "list them with `athena --profile {profile} patients list`"
        )));
    }
    if chosen.len() > 1 && !args.all {
        return Err(CliError::Usage(format!(
            "{} signed-in patients match: name one with --patient (and --org), or pass --all",
            chosen.len()
        )));
    }
    let http = BlockingTransport::new()?;
    let store = ctx.sessions(&profile)?;
    let legacy = session::credential_store();
    let lock = RefreshLock::for_binary(BIN)?;
    let mut done = Vec::new();
    let mut incomplete = Vec::new();
    // The first v0.2.x item that could not be removed: the command ends with
    // its code once its one document is out.
    let mut left_behind: Option<CliError> = None;
    for entry in chosen {
        let result = logout::sign_out(&http, &lock, &*store, &legacy, &entry.org, &entry.patient)?;
        let mut row = json!({
            "org": entry.org,
            "patient": entry.patient,
            "label": entry.label,
            "credential_removed": result.credential_removed,
            "revoked": result.revoked,
        });
        match result.left_behind {
            None => {
                ctx.update_index(|index| {
                    Ok(index.remove(Some(profile.as_str()), &entry.org, &entry.patient))
                })?;
                done.push(row);
            }
            Some(e) => {
                // The patient stays in the index, so a second `auth logout`
                // retries the item.
                eprintln!(
                    "warning: {}. Run `athena --profile {profile} auth logout` again and allow \
                     the Keychain prompt, or remove it with `security delete-generic-password \
                     -s {} -a <org>/<patient>`",
                    e.message(),
                    session::service()
                );
                row["error"] = json!({"code": e.code(), "message": e.message()});
                incomplete.push(row);
                left_behind.get_or_insert(e);
            }
        }
    }
    if ctx.json {
        output::json(&json!({
            "schema": "auth-logout/v1",
            "profile": profile.as_str(),
            "signed_out": done,
            "incomplete": incomplete,
        }));
    } else {
        for row in &done {
            println!(
                "Signed out {} at {}{}.",
                row["patient"].as_str().unwrap_or_default(),
                row["org"].as_str().unwrap_or_default(),
                if row["revoked"] == true {
                    " (tokens revoked)"
                } else {
                    ""
                }
            );
        }
        for row in &incomplete {
            println!(
                "Did not finish signing out {} at {}: its v0.2.x sign-in is still in the \
                 Keychain. Run `athena --profile {profile} auth logout` again.",
                row["patient"].as_str().unwrap_or_default(),
                row["org"].as_str().unwrap_or_default(),
            );
        }
    }
    if let Some(e) = left_behind {
        std::process::exit(e.exit_code());
    }
    Ok(())
}

// ---------------------------------------------------------------- profiles

fn profiles_cmd(ctx: &Ctx, cmd: ProfilesCmd) -> Result<(), CliError> {
    match cmd {
        ProfilesCmd::List => profiles_list(ctx),
        ProfilesCmd::Add { name } => profiles_add(ctx, &name),
        ProfilesCmd::Remove { name, force } => profiles_remove(ctx, &name, force),
        ProfilesCmd::Migrate(args) => profiles_migrate(ctx, args),
    }
}

/// A sign-in in no profile, as `profiles list` shows it.
fn unassigned_row(e: &SessionEntry) -> Value {
    let mut row = Map::new();
    row.insert("org".into(), json!(e.org));
    row.insert("patient".into(), json!(e.patient));
    if let Some(label) = &e.label {
        row.insert("label".into(), json!(label));
    }
    Value::Object(row)
}

fn profiles_list(ctx: &Ctx) -> Result<(), CliError> {
    let config = ctx.load()?;
    let index = ctx.index()?;
    let unassigned: Vec<Value> = index
        .matching(None, None, None)
        .into_iter()
        .map(unassigned_row)
        .collect();
    let list = config.profiles.list();
    if ctx.json {
        let mut value = serde_json::to_value(&list)
            .map_err(|e| CliError::Other(format!("encoding the profile list: {e}")))?;
        if let (Value::Object(map), false) = (&mut value, unassigned.is_empty()) {
            map.insert("unassigned".into(), Value::Array(unassigned));
        }
        output::json(&value);
        return Ok(());
    }
    if list.profiles.is_empty() {
        println!("No profiles. Add one per person who signs in: `athena profiles add <NAME>`.");
    } else {
        let rows: Vec<Value> = list
            .profiles
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "sign_ins": index.matching(Some(&p.name), None, None).len(),
                })
            })
            .collect();
        output::table(&output::table_view(&rows, &["name", "sign_ins"]));
    }
    if !unassigned.is_empty() {
        println!();
        println!(
            "Stored before v0.4.0, in no profile (move each with `athena profiles migrate <NAME> \
             --org <ORG> --patient <ID|LABEL>`):"
        );
        output::table(&output::table_view(
            &unassigned,
            &["org", "patient", "label"],
        ));
    }
    Ok(())
}

fn profiles_add(ctx: &Ctx, name: &str) -> Result<(), CliError> {
    let name = ProfileName::parse(name)?;
    let mut config = ctx.load()?;
    let added = config.profiles.add(name.clone());
    if added {
        ctx.save(&config)?;
    }
    if ctx.json {
        output::json(&json!({
            "schema": "profile-add/v1",
            "profile": name.as_str(),
            "added": added,
        }));
    } else if added {
        println!(
            "Added profile {name}. Sign in with `athena --profile {name} auth login --org <ORG>`."
        );
    } else {
        println!("Profile {name} is already configured.");
    }
    Ok(())
}

fn profiles_remove(ctx: &Ctx, name: &str, force: bool) -> Result<(), CliError> {
    let name = ProfileName::parse(name)?;
    let config = ctx.load()?;
    if !config.profiles.contains(&name) {
        return Err(CliError::NotFound(format!("no profile `{name}`"))
            .with_remedy("list them with `athena profiles list`"));
    }
    let entries: Vec<SessionEntry> = ctx
        .index()?
        .matching(Some(name.as_str()), None, None)
        .into_iter()
        .cloned()
        .collect();
    let prompt = format!(
        "Remove profile {name}, signing out its {} sign-in{} and revoking their tokens?",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    confirm::require_confirmable(force, ctx.interactive, &prompt)?;
    confirm::confirm(force, &prompt)?;

    let http = BlockingTransport::new()?;
    let store = ctx.sessions(&name)?;
    let legacy = session::credential_store();
    let lock = RefreshLock::for_binary(BIN)?;
    let mut revoked = 0;
    let sign_out = |org: &str, patient: &str| -> Result<bool, CliError> {
        let result = logout::sign_out(&http, &lock, &*store, &legacy, org, patient)?;
        match result.left_behind {
            // The profile stays, so `profiles remove` can be run again.
            Some(e) => Err(e.with_remedy(format!(
                "run `athena profiles remove {name}` again and allow the Keychain prompt"
            ))),
            None => Ok(result.revoked),
        }
    };
    for entry in &entries {
        revoked += usize::from(sign_out(&entry.org, &entry.patient)?);
        ctx.update_index(|index| {
            Ok(index.remove(Some(name.as_str()), &entry.org, &entry.patient))
        })?;
    }
    // Sign-ins in the vault that the index does not name are signed out the
    // same way, revoked first, rather than deleted with their tokens live,
    // and counted as they are removed.
    let vault = ctx.vault()?;
    let mut not_in_index = 0;
    for account in ProfileSessions::new(&vault, name.clone()).accounts()? {
        let result = logout::sign_out_account(&http, &lock, &*store, &account)?;
        not_in_index += usize::from(result.credential_removed);
        revoked += usize::from(result.revoked);
    }
    let signed_out = entries.len() + not_in_index;
    let browser_profile_removed =
        pk_cli_browser::forget_named_profile(BIN, name.as_str()).map_err(CliError::from)?;
    let mut config = ctx.load()?;
    config.profiles.remove(&name);
    ctx.save(&config)?;
    if ctx.json {
        output::json(&json!({
            "schema": "profile-remove/v1",
            "profile": name.as_str(),
            "signed_out": signed_out,
            "revoked": revoked,
            "not_in_index": not_in_index,
            "browser_profile_removed": browser_profile_removed,
        }));
    } else {
        println!(
            "Removed profile {name}: {signed_out} sign-in{} signed out, {revoked} of them \
             revoked{}.",
            if signed_out == 1 { "" } else { "s" },
            if browser_profile_removed {
                ", its browser profile removed"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// One `--patient` of `profiles migrate`: the sign-in to move, or none when
/// an earlier run moved it already.
fn migration_source(
    index: &SessionIndex,
    profile: &ProfileName,
    org: &str,
    patient: &str,
) -> Result<Option<SessionEntry>, CliError> {
    let old = index.matching(None, Some(patient), Some(org));
    let moved = index.matching(Some(profile.as_str()), Some(patient), Some(org));
    match (old.len(), moved.len()) {
        (1, _) => Ok(Some(old[0].clone())),
        (0, 0) => Err(CliError::NotFound(format!(
            "no sign-in from before v0.4.0 at `{org}` has the id or label `{patient}`"
        ))
        .with_remedy("list them with `athena profiles list`")),
        (0, _) => Ok(None),
        (_, _) => Err(CliError::Usage(format!(
            "more than one sign-in at `{org}` matches `{patient}`: name it by its patient id"
        ))),
    }
}

fn profiles_migrate(ctx: &Ctx, args: MigrateCmd) -> Result<(), CliError> {
    migrate_with(
        ctx,
        args,
        &session::credential_store(),
        &RefreshLock::for_binary(BIN)?,
        &BlockingTransport::new()?,
    )
}

/// `profiles migrate`, moving items out of `from` under `lock`, and revoking
/// through `http` a sign-in the move replaces.
fn migrate_with(
    ctx: &Ctx,
    args: MigrateCmd,
    from: &dyn KeyStore,
    lock: &RefreshLock,
    http: &dyn crate::http::Http,
) -> Result<(), CliError> {
    let name = ProfileName::parse(&args.name)?;
    config::validate_slug(&args.org)?;
    for patient in &args.patients {
        if patient.is_empty() {
            return Err(CliError::Usage("--patient needs an id or a label".into()));
        }
    }
    let index = ctx.index()?;
    let mut chosen: Vec<SessionEntry> = Vec::new();
    let mut already = 0;
    for patient in &args.patients {
        match migration_source(&index, &name, &args.org, patient)? {
            Some(entry) if !chosen.contains(&entry) => chosen.push(entry),
            Some(_) => {}
            None => already += 1,
        }
    }
    // Labels stay distinct within the profile, so --patient <LABEL> names one.
    for entry in &chosen {
        if let Some(label) = &entry.label {
            if index.label_taken(Some(name.as_str()), &entry.org, label, Some(&entry.patient)) {
                return Err(CliError::Usage(format!(
                    "the label `{label}` already names a sign-in at `{}` in profile `{name}`",
                    entry.org
                ))
                .with_remedy(format!(
                    "relabel that one first: `athena --profile {name} auth relabel --org {} \
                     --from {label} --to <LABEL>`",
                    entry.org
                )));
            }
        }
    }

    let mut config = ctx.load()?;
    let added = config.profiles.add(name.clone());
    if added {
        ctx.save(&config)?;
    }
    let vault = ctx.vault()?;
    let sessions = ProfileSessions::new(&vault, name.clone());
    let mut totals = Migration::default();
    let mut rows = Vec::new();
    let mut replaced_revoked = 0;
    for entry in chosen {
        let account = entry.key();
        // When the profile holds this sign-in already, both copies are read
        // first, so the one the move does not keep can be revoked. (Only
        // then: reading the old item is a Keychain prompt.)
        let held = sessions.load(&account).ok().flatten();
        let old = match held {
            Some(_) => from
                .get(&account)?
                .and_then(|text| pk_med_smart::session::decode_session(text.expose()).ok()),
            None => None,
        };
        // One account per call, so each sign-in's outcome is known.
        let done = migrate_sessions(from, &sessions, std::slice::from_ref(&account), lock)?;
        totals.moved += done.moved;
        totals.kept += done.kept;
        totals.absent += done.absent;
        let outcome = if done.moved > 0 {
            "moved"
        } else if done.kept > 0 {
            "kept"
        } else {
            "absent"
        };
        let login = if outcome == "absent" {
            None
        } else {
            sessions
                .load(&entry.key())
                .ok()
                .flatten()
                .as_ref()
                .and_then(session::login_fingerprint)
        };
        ctx.update_index(|index| {
            let in_profile = index
                .find_mut(Some(name.as_str()), &entry.org, &entry.patient)
                .is_some();
            if outcome == "absent" || in_profile {
                // Nothing was stored for it, or the profile has its own entry.
                index.remove(None, &entry.org, &entry.patient);
            } else if let Some(e) = index.find_mut(None, &entry.org, &entry.patient) {
                e.profile = Some(name.to_string());
                e.login = login.clone();
            }
            Ok(())
        })?;
        let mut row = unassigned_row(&entry);
        row["outcome"] = json!(outcome);
        if let (Some(held), Some(old)) = (held, old) {
            let (kept, dropped) = if done.moved > 0 {
                (old, held)
            } else {
                (held, old)
            };
            if let Some(revoked) = logout::revoke_replaced(http, &kept, &dropped) {
                replaced_revoked += usize::from(revoked);
                row["replaced"] = json!({ "revoked": revoked });
            }
        }
        rows.push(row);
    }
    let logins = distinct_logins(&ctx.index()?, &name);
    if logins > 1 {
        eprintln!(
            "warning: profile {name}'s sign-ins came from {logins} different athenahealth \
             logins. A profile is one person's: sign out the others with `athena --profile \
             {name} auth logout --org <ORG> --patient <ID|LABEL>`, and move or sign them in to \
             their own profile."
        );
    }
    if ctx.json {
        output::json(&json!({
            "schema": "profile-migrate/v1",
            "profile": name.as_str(),
            "added": added,
            "moved": totals.moved,
            "kept": totals.kept,
            "absent": totals.absent,
            "already_in_profile": already,
            "replaced_revoked": replaced_revoked,
            "logins": logins,
            "sign_ins": rows,
        }));
    } else {
        println!(
            "Profile {name}{}: {} moved, {} already in it, {} with nothing stored (sign in again).",
            if added { " (added)" } else { "" },
            totals.moved,
            totals.kept + already,
            totals.absent
        );
        if !rows.is_empty() {
            output::table(&output::table_view(
                &rows,
                &["org", "patient", "label", "outcome"],
            ));
        }
    }
    Ok(())
}

/// How many logins a profile's sign-ins came from, by fingerprint, counted
/// per environment (each has an issuer of its own).
fn distinct_logins(index: &SessionIndex, profile: &ProfileName) -> usize {
    use std::collections::{BTreeMap, BTreeSet};
    let mut per_environment: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for entry in index.matching(Some(profile.as_str()), None, None) {
        if let Some(login) = entry.login.as_deref() {
            per_environment
                .entry(entry.environment.to_string())
                .or_default()
                .insert(login);
        }
    }
    per_environment
        .values()
        .map(BTreeSet::len)
        .max()
        .unwrap_or(0)
}

// ---------------------------------------------------------------- config

fn config_cmd(ctx: &Ctx, cmd: ConfigCmd) -> Result<(), CliError> {
    match cmd {
        ConfigCmd::Path => {
            let path = ctx.config_path()?;
            if ctx.json {
                output::json(
                    &json!({"schema": "config-path/v1", "path": path.display().to_string()}),
                );
            } else {
                println!("{}", path.display());
            }
            Ok(())
        }
        ConfigCmd::Show => {
            let config = ctx.load()?;
            let value = serde_json::to_value(&config).unwrap_or(Value::Null);
            output::emit_one(ctx.json, "config", value);
            Ok(())
        }
        ConfigCmd::Set(set) => {
            let mut config = ctx.load()?;
            let what = match set {
                SetCmd::ClientId { id, env } => {
                    config::validate_client_id(&id)?;
                    config.client_ids.set(env, Some(id));
                    format!("client id for {env}")
                }
                SetCmd::RedirectPorts { ports } => {
                    config.redirect_ports = Some(config::parse_ports(&ports)?);
                    "redirect ports".to_string()
                }
                SetCmd::ScopeVersion { version } => {
                    config.scope_version = Some(version);
                    "scope version".to_string()
                }
                SetCmd::Practice(args) => {
                    config::validate_slug(&args.slug)?;
                    let mut practice =
                        config
                            .practices
                            .get(&args.slug)
                            .cloned()
                            .unwrap_or_else(|| {
                                if args.slug == config::SANDBOX_ORG {
                                    Practice::sandbox()
                                } else {
                                    Practice::default()
                                }
                            });
                    if let Some(v) = args.name {
                        practice.name = Some(v);
                    }
                    if let Some(v) = args.env {
                        practice.environment = v;
                    }
                    if let Some(v) = args.practice_id {
                        practice.practice_id = Some(v);
                    }
                    if let Some(v) = args.brand_id {
                        practice.brand_id = Some(v);
                    }
                    if let Some(v) = args.csg_id {
                        practice.csg_id = Some(v);
                    }
                    if let Some(v) = args.portal_host {
                        practice.portal_host = Some(v);
                    }
                    if let Some(v) = args.fhir_base {
                        practice.fhir_base = Some(v.trim_end_matches('/').to_string());
                    }
                    if let Some(v) = args.client_id {
                        config::validate_client_id(&v)?;
                        practice.client_id = Some(v);
                    }
                    practice.validate()?;
                    config.practices.insert(args.slug.clone(), practice);
                    format!("practice `{}`", args.slug)
                }
            };
            ctx.save(&config)?;
            config_saved(ctx, &config, &what)
        }
        ConfigCmd::Unset(unset) => {
            let mut config = ctx.load()?;
            let what = match unset {
                UnsetCmd::ClientId { env } => {
                    config.client_ids.set(env, None);
                    format!("client id for {env}")
                }
                UnsetCmd::RedirectPorts => {
                    config.redirect_ports = None;
                    "redirect ports".to_string()
                }
                UnsetCmd::ScopeVersion => {
                    config.scope_version = None;
                    "scope version".to_string()
                }
                UnsetCmd::Practice { slug } => {
                    config::validate_slug(&slug)?;
                    if config.practices.remove(&slug).is_none() {
                        return Err(CliError::NotFound(format!(
                            "no practice profile `{slug}` is configured"
                        )));
                    }
                    format!("practice `{slug}`")
                }
            };
            ctx.save(&config)?;
            config_saved(ctx, &config, &what)
        }
    }
}

fn config_saved(ctx: &Ctx, config: &Config, what: &str) -> Result<(), CliError> {
    if ctx.json {
        let value = serde_json::to_value(config).unwrap_or(Value::Null);
        output::json(&output::tagged("config", value));
    } else {
        println!("Saved {what} to {}.", ctx.config_path()?.display());
    }
    Ok(())
}

// ---------------------------------------------------------------- orgs

fn orgs(ctx: &Ctx, cmd: OrgsCmd) -> Result<(), CliError> {
    match cmd {
        OrgsCmd::List => {
            let config = ctx.load()?;
            let items: Vec<Value> = config
                .orgs()
                .into_iter()
                .map(|org| {
                    let p = &org.practice;
                    let mut row = Map::new();
                    row.insert("id".into(), json!(org.slug));
                    row.insert("name".into(), json!(p.display_name(&org.slug)));
                    row.insert("vendor".into(), json!(stream::VENDOR));
                    row.insert("environment".into(), json!(p.environment));
                    for (k, v) in [
                        ("practice_id", &p.practice_id),
                        ("brand_id", &p.brand_id),
                        ("csg_id", &p.csg_id),
                        ("portal_host", &p.portal_host),
                    ] {
                        if let Some(v) = v {
                            row.insert(k.into(), json!(v));
                        }
                    }
                    if let Ok(base) = p.fhir_base() {
                        row.insert("fhir_base".into(), json!(base));
                    }
                    row.insert(
                        "client_id_configured".into(),
                        json!(config.client_id_for(p).is_ok()),
                    );
                    row.insert("builtin".into(), json!(org.builtin));
                    Value::Object(row)
                })
                .collect();
            output::emit_list(
                ctx.json,
                "org",
                items,
                &[
                    "id",
                    "name",
                    "environment",
                    "practice_id",
                    "client_id_configured",
                ],
            );
            Ok(())
        }
        OrgsCmd::Search { text, limit } => {
            let query = text.join(" ");
            if query.trim().is_empty() {
                return Err(CliError::Usage("search needs some text".into()));
            }
            let path = ctx.directory_path()?;
            let dir = Directory::load(&path)?.ok_or_else(|| {
                CliError::NotFound("athenahealth's practice directory is not downloaded yet".into())
                    .with_remedy("run `athena orgs refresh` (about 135 MB)")
            })?;
            let found = dir.search(&query);
            let total = found.len();
            let items = found
                .into_iter()
                .take(limit)
                .map(directory_item)
                .collect::<Result<Vec<Value>, CliError>>()?;
            if ctx.json {
                let mut page = Paged::new("org", items);
                page.total = Some(total as u64);
                output::json(&serde_json::to_value(&page).unwrap_or(Value::Null));
            } else {
                output::table(&output::table_view(
                    &items,
                    &["name", "city", "state", "practice_id", "brand_id", "csg_id"],
                ));
                if total > limit {
                    eprintln!("{total} matches; showing {limit} (--limit N for more)");
                }
                ctx.note("Add one with: athena config set practice <SLUG> --practice-id <ID> --brand-id <ID> --csg-id <ID>");
            }
            Ok(())
        }
        OrgsCmd::Refresh { url } => {
            pk_cli_core::transport::require_secure_url(&url)?;
            ctx.note("Downloading athenahealth's practice directory (about 135 MB)...");
            let dir = directory::download(&url)?;
            let path = ctx.directory_path()?;
            dir.save(&path)?;
            let value = json!({
                "entries": dir.entries.len(),
                "published_at": dir.published_at,
                "fetched_at": dir.fetched_at,
                "path": path.display().to_string(),
            });
            output::emit(ctx.json, "org-directory", value, |v| {
                println!(
                    "{} practices cached in {}.",
                    v["entries"],
                    v["path"].as_str().unwrap_or_default()
                );
            });
            Ok(())
        }
    }
}

/// A directory entry as an `org-list/v1` item: health-records/v1's
/// `Organization` fields (`id`, `name`, `fhir_base`, `vendor`, `location`),
/// then the ids `config set practice` takes. Its `id` is the directory's key,
/// `<practice>-<brand>-<csg>`, not a practice profile: `--org` takes the slug
/// a profile is saved under.
fn directory_item(e: &directory::Entry) -> Result<Value, CliError> {
    let mut row = Map::new();
    let ids: Vec<&str> = [&e.practice_id, &e.brand_id, &e.csg_id]
        .into_iter()
        .filter_map(|v| v.as_deref())
        .collect();
    let id = if ids.is_empty() {
        pk_cli_documents::fs_safe(&e.name.to_lowercase())
    } else {
        ids.join("-")
    };
    row.insert("id".into(), json!(id));
    // Without a brand endpoint the practice is reached through athenahealth's
    // global base, as a profile with no ids is.
    let base = e
        .fhir_base
        .clone()
        .unwrap_or_else(|| format!("{}/fhir/r4", crate::env::PRODUCTION_API));
    row.insert("fhir_base".into(), json!(base));
    row.insert("vendor".into(), json!(stream::VENDOR));
    let location = match (&e.city, &e.state) {
        (Some(city), Some(state)) => Some(format!("{city}, {state}")),
        (Some(one), None) | (None, Some(one)) => Some(one.clone()),
        (None, None) => None,
    };
    if let Some(location) = location {
        row.insert("location".into(), json!(location));
    }
    let fields = serde_json::to_value(e)
        .map_err(|err| CliError::Other(format!("encoding a directory entry: {err}")))?;
    if let Value::Object(fields) = fields {
        for (k, v) in fields {
            row.entry(k).or_insert(v);
        }
    }
    Ok(Value::Object(row))
}

fn patients_list(ctx: &Ctx, org: Option<&str>) -> Result<(), CliError> {
    if let Some(org) = org {
        config::validate_slug(org)?;
    }
    let profile = ctx.profile(&ctx.load()?)?;
    let index = ctx.index()?;
    let unassigned = index.unassigned();
    if unassigned > 0 {
        ctx.note(unassigned_note(unassigned));
    }
    let items: Vec<Value> = index
        .matching(Some(profile.as_str()), None, org)
        .into_iter()
        .map(|e| {
            let mut row = Map::new();
            row.insert("id".into(), json!(e.patient));
            row.insert("organization".into(), json!(e.org));
            if let Some(label) = &e.label {
                row.insert("label".into(), json!(label));
            }
            if let Some(rel) = &e.relationship {
                row.insert("relationship".into(), json!(rel));
            }
            row.insert("kind".into(), json!(stream::kind::PATIENT));
            row.insert("session".into(), json!(e.key()));
            row.insert("environment".into(), json!(e.environment));
            row.insert("stored_at".into(), json!(e.stored_at));
            if let Some(at) = &e.signin_expires_at {
                row.insert("signin_expires_at".into(), json!(at));
            }
            Value::Object(row)
        })
        .collect();
    output::emit_list(
        ctx.json,
        "patient",
        items,
        &[
            "id",
            "organization",
            "label",
            "relationship",
            "signin_expires_at",
        ],
    );
    Ok(())
}

// ---------------------------------------------------------------- reads

/// A selected sign-in: its profile's store, its index entry and its session.
struct Opened {
    store: Store,
    entry: SessionEntry,
    stored: SmartSession,
}

/// The indexed sign-in `select` names in the command's profile, without
/// touching the keychain or the network.
fn resolve_entry(ctx: &Ctx, select: &Select) -> Result<(ProfileName, SessionEntry), CliError> {
    if let Some(org) = &select.org {
        config::validate_slug(org)?;
    }
    let profile = ctx.profile(&ctx.load()?)?;
    let entry = ctx.index()?.resolve(
        profile.as_str(),
        select.patient.as_deref(),
        select.org.as_deref(),
    )?;
    Ok((profile, entry))
}

/// The selected sign-in read from the vault, before any network.
fn open_session(ctx: &Ctx, select: &Select) -> Result<Opened, CliError> {
    let (profile, entry) = resolve_entry(ctx, select)?;
    let store = ctx.sessions(&profile)?;
    let stored = session::load(&*store, &entry)?;
    Ok(Opened {
        store,
        entry,
        stored,
    })
}

/// The note for sign-ins stored before v0.4.0, which are in no profile.
fn unassigned_note(count: usize) -> String {
    format!(
        "note: {count} sign-in{} stored before v0.4.0 {} in no profile; `athena profiles list` \
         shows them, and `athena profiles migrate <NAME> --org <ORG> --patient <ID|LABEL>` moves \
         each into its person's profile",
        if count == 1 { "" } else { "s" },
        if count == 1 { "is" } else { "are" }
    )
}

/// Whether a grant answered, or was refused, between `before` and `after`.
fn renewed(before: &SmartSession, after: &SmartSession) -> bool {
    before.tokens.obtained_at != after.tokens.obtained_at || before.refused_at != after.refused_at
}

/// A read client over a stored sign-in, its token renewed first when it has
/// lapsed or was refused. A refused renewal is recorded in the index.
fn start_client(ctx: &Ctx, opened: &Opened) -> Result<Client, CliError> {
    let Opened {
        store,
        entry,
        stored,
    } = opened;
    let started = BlockingTransport::new().and_then(|http| {
        let live = Live::start_in(
            entry.profile.as_deref(),
            store.clone(),
            RefreshLock::for_binary(BIN)?,
            http.clone(),
            stored.clone(),
        )?;
        Client::new(http, live)
    });
    match started {
        Ok(client) if ctx.quiet => Ok(client),
        Ok(client) => Ok(client.on_retry(|line| eprintln!("{line}"))),
        Err(e) => {
            if e.reason() == Some(Reason::CredentialRefused) {
                let mut refused = stored.clone();
                refused.refused_at = Some(unix_secs());
                note_renewal(ctx, entry, stored, &refused);
            }
            Err(e)
        }
    }
}

/// Keep the index's expiry in step after a command renewed the token, or
/// athenahealth refused the renewal.
fn note_renewal(ctx: &Ctx, entry: &SessionEntry, before: &SmartSession, after: &SmartSession) {
    if !renewed(before, after) {
        return;
    }
    // Only the expiry changes, on the entry as it is now: a relabel or a
    // sign-out since this command started stands. Best effort: the vault
    // already holds the renewal.
    let _ = ctx.update_index(|index| {
        if let Some(current) = index.find_mut(entry.profile.as_deref(), &entry.org, &entry.patient)
        {
            current.signin_expires_at = Some(rfc3339(session::signin_expires_at(after)));
        }
        Ok(())
    });
}

fn records_export(ctx: &Ctx, args: ExportCmd) -> Result<(), CliError> {
    if let Some(since) = &args.since {
        dates::parse_iso(since)?;
    }
    if !args.ndjson && args.out.is_none() {
        return Err(CliError::Usage(
            "records export writes a stream: pass --ndjson for stdout, or -o <DIR>".into(),
        ));
    }
    if args.ndjson {
        // stdout is the stream: an error goes to stderr alone, whatever
        // --json says, so stdout holds nothing that is not the stream.
        if let Err(e) = export_to_stdout(ctx, &args) {
            eprintln!("error: {e}");
            std::process::exit(e.exit_code());
        }
        return Ok(());
    }
    export_to_dir(ctx, &args)
}

fn stream_header(ctx: &Ctx, entry: &SessionEntry, since: Option<&str>) -> Result<Header, CliError> {
    let name = ctx
        .load()?
        .practices
        .get(&entry.org)
        .map(|p| p.display_name(&entry.org))
        .unwrap_or_else(|| {
            if entry.org == config::SANDBOX_ORG {
                Practice::sandbox().display_name(&entry.org)
            } else {
                entry.org.clone()
            }
        });
    Ok(stream::header(
        stream::Organization {
            id: entry.org.clone(),
            name,
            fhir_base: entry.fhir_base.clone(),
            vendor: Some(stream::VENDOR.into()),
            location: None,
            aliases: Vec::new(),
        },
        stream::PatientContext {
            id: entry.patient.clone(),
            organization: entry.org.clone(),
            label: entry.label.clone(),
            relationship: entry.relationship.clone(),
            kind: Some(stream::kind::PATIENT.into()),
            member_role: None,
            fhir_user: None,
            name: None,
            session: None,
        },
        since.map(str::to_string),
        dates::fmt_rfc3339(unix_now()),
    ))
}

/// What an export needs before its first byte, the token renewed when due:
/// a failure here writes nothing anywhere.
struct Prepared {
    entry: SessionEntry,
    stored: SmartSession,
    header: Header,
    client: Client,
}

fn prepare_export(ctx: &Ctx, args: &ExportCmd) -> Result<Prepared, CliError> {
    let opened = open_session(ctx, &args.select)?;
    let header = stream_header(ctx, &opened.entry, args.since.as_deref())?;
    let client = start_client(ctx, &opened)?;
    Ok(Prepared {
        entry: opened.entry,
        stored: opened.stored,
        header,
        client,
    })
}

fn now_rfc3339() -> String {
    dates::fmt_rfc3339(unix_now())
}

fn unix_secs() -> u64 {
    unix_now().max(0) as u64
}

/// Unix seconds as RFC 3339.
fn rfc3339(at: u64) -> String {
    dates::fmt_rfc3339(at as i64)
}

fn export_to_stdout(ctx: &Ctx, args: &ExportCmd) -> Result<(), CliError> {
    let Prepared {
        entry,
        stored,
        header,
        client,
    } = prepare_export(ctx, args)?;
    let since = args.since.as_deref();
    let stdout = std::io::stdout();
    let written = export::write_stream(BufWriter::new(stdout.lock()), &header, now_rfc3339, |w| {
        export::export(&client, w, since)
    });
    note_renewal(ctx, &entry, &stored, &client.session());
    let (_, error) = written?;
    error.map_or(Ok(()), Err)
}

fn export_to_dir(ctx: &Ctx, args: &ExportCmd) -> Result<(), CliError> {
    let dir = args.out.clone().unwrap_or_default();
    // Resolve the session first, so a bad --patient creates no file.
    let (profile, entry) = resolve_entry(ctx, &args.select)?;
    if !synthetic(&entry.fhir_base) {
        let who = entry.label.clone().unwrap_or_else(|| entry.patient.clone());
        return Err(CliError::Usage(format!(
            "-o writes records to a plain file, so it is for athenahealth's sandbox and test \
             servers only; patient {who} is at {}",
            entry.org
        ))
        .with_remedy(format!(
            "pipe them into medrec instead: `medrec import --from {BIN} --from-profile {profile} \
             --patient {who}`, which runs `{BIN} --profile {profile} records export --patient \
             {who} --ndjson`"
        )));
    }
    let Prepared {
        entry,
        stored,
        header,
        client,
    } = prepare_export(ctx, args)?;
    let since = args.since.as_deref();
    let written = export::write_dir(&dir, &header, now_rfc3339, |w| {
        export::export(&client, w, since)
    });
    note_renewal(ctx, &entry, &stored, &client.session());
    let (export, error) = written?;
    // One records-export/v1 document, complete or not (medical-common
    // DESIGN 1.5); a pull cut short then exits with its error's code.
    if ctx.json {
        let value = serde_json::to_value(&export)
            .map_err(|e| CliError::Other(format!("encoding records-export/v1: {e}")))?;
        output::json(&value);
    } else {
        let total: u64 = export.counts.values().sum();
        println!(
            "Wrote {total} resources ({} skipped, {} bytes) to {}{}.",
            export.skipped.len(),
            export.bytes,
            export.path,
            if export.complete {
                ""
            } else {
                ", ending with complete: false"
            }
        );
    }
    if let Some(e) = error {
        eprintln!("error: {e}");
        std::process::exit(e.exit_code());
    }
    Ok(())
}

/// Whether a FHIR base holds synthetic patients only: a server on this
/// machine, or athenahealth's Preview sandbox (its global base, or the
/// sandbox practices 80000 and 195900). Only these may be written to a
/// plain file with `-o`.
pub fn synthetic(fhir_base: &str) -> bool {
    let Some(origin) = crate::http::origin(fhir_base) else {
        return false;
    };
    if origin.starts_with("http://127.0.0.1:") || origin.starts_with("http://localhost:") {
        return true;
    }
    let preview = Environment::Preview
        .api_base()
        .and_then(crate::http::origin);
    if preview.as_deref() != Some(origin.as_str()) {
        return false;
    }
    let path = fhir_base[origin.len()..].trim_end_matches('/');
    path == "/fhir/r4" || path.starts_with("/80000/") || path.starts_with("/195900/")
}

fn create_private_dir(dir: &Path) -> Result<(), CliError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| CliError::Other(format!("creating {}: {e}", dir.display())))
}

/// Write bytes to a new file, or replace a file athena wrote before, owner-only.
fn save_file(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    session::write_private(path, bytes)
}

fn documents_cmd(ctx: &Ctx, cmd: DocumentsCmd) -> Result<(), CliError> {
    match cmd {
        DocumentsCmd::List { select, range } => {
            range.validate()?;
            let opened = open_session(ctx, &select)?;
            let client = start_client(ctx, &opened)?;
            let (entry, stored) = (opened.entry, opened.stored);
            let listing = documents::list(&client, &range);
            note_renewal(ctx, &entry, &stored, &client.session());
            let listing = listing?;
            warn_withheld(&listing);
            let docs: Vec<_> = listing.items.into_iter().map(|i| i.document).collect();
            let page = Paged::new("document", docs);
            if ctx.json {
                let value = serde_json::to_value(&page).unwrap_or(Value::Null);
                output::json(&documents::with_withheld(value, &listing.withheld));
            } else {
                page.emit(false);
            }
            Ok(())
        }
        DocumentsCmd::Download {
            id,
            all,
            out,
            select,
            range,
        } => {
            range.validate()?;
            if let Some(id) = &id {
                documents::parse_id(id)?;
            }
            // A file's directory must exist; a directory `-o` names is made.
            if !all && !documents::is_dir_target(&out) {
                let parent = out.parent().filter(|p| !p.as_os_str().is_empty());
                if let Some(parent) = parent.filter(|p| !p.is_dir()) {
                    return Err(if parent.exists() {
                        CliError::Usage(format!("{} is a file, not a directory", parent.display()))
                            .with_remedy("choose another path for -o")
                    } else {
                        CliError::Usage(format!("{} does not exist", parent.display())).with_remedy(
                            "create it first, or end -o with / to have athena create it as a \
                             directory",
                        )
                    });
                }
            }
            let opened = open_session(ctx, &select)?;
            if (all || documents::is_dir_target(&out)) && !out.is_dir() {
                create_private_dir(&out)?;
            }
            let client = start_client(ctx, &opened)?;
            let result = if all {
                download_all(ctx, &client, &out, &range)
            } else {
                download_one(ctx, &client, id.as_deref().unwrap_or_default(), &out)
            };
            note_renewal(ctx, &opened.entry, &opened.stored, &client.session());
            result
        }
    }
}

fn download_one(ctx: &Ctx, client: &Client, id: &str, out: &Path) -> Result<(), CliError> {
    let item = documents::find(client, id)?;
    let fetched = documents::fetch(client, &item).map_err(|f| f.error)?;
    let path = documents::target(out, &item, fetched.content_type.as_deref());
    save_file(&path, &fetched.bytes)?;
    let saved = SavedDocument::from_document(
        &item.document,
        path.display().to_string(),
        fetched.bytes.len() as u64,
    );
    if ctx.json {
        output::json(&serde_json::to_value(&saved).unwrap_or(Value::Null));
    } else {
        println!(
            "Saved {} ({} bytes) to {}.",
            saved.id, saved.bytes, saved.path
        );
    }
    Ok(())
}

fn warn_withheld(listing: &documents::Listing) {
    for w in &listing.withheld {
        eprintln!(
            "{}",
            documents::withheld_warning(w.resource_type, &w.reason)
        );
    }
}

fn download_all(ctx: &Ctx, client: &Client, dir: &Path, range: &RangeArgs) -> Result<(), CliError> {
    let listing = documents::list(client, range)?;
    warn_withheld(&listing);
    let withheld = listing.withheld;
    let items = listing.items;
    let mut saved = Vec::new();
    let mut skipped = Vec::new();
    for item in items {
        match documents::fetch(client, &item) {
            Ok(fetched) => {
                let path = dir.join(documents::file_name(&item, fetched.content_type.as_deref()));
                save_file(&path, &fetched.bytes)?;
                saved.push(SavedDocument::from_document(
                    &item.document,
                    path.display().to_string(),
                    fetched.bytes.len() as u64,
                ));
            }
            // A refused sign-in stops the batch: every other file would fail too.
            Err(f) if f.error.is_auth() && f.error.reason() != Some(Reason::Forbidden) => {
                return Err(f.error)
            }
            Err(f) => skipped.push(
                SkippedDocument::new(item.document.id.clone(), f.error.message().to_string())
                    .with_code(f.code),
            ),
        }
    }
    let batch = DownloadBatch::new(dir.display().to_string(), saved).with_skipped(skipped);
    if ctx.json {
        let value = serde_json::to_value(&batch).unwrap_or(Value::Null);
        output::json(&documents::with_withheld(value, &withheld));
    } else {
        println!(
            "Saved {} documents ({} bytes) to {}{}.",
            batch.count,
            batch.bytes_total,
            batch.dir,
            if batch.skipped.is_empty() {
                String::new()
            } else {
                format!("; skipped {}", batch.skipped.len())
            }
        );
        for s in &batch.skipped {
            eprintln!(
                "skipped {} ({}): {}",
                s.id,
                s.code.as_deref().unwrap_or(""),
                s.reason
            );
        }
    }
    Ok(())
}

fn api(ctx: &Ctx, args: ApiCmd) -> Result<(), CliError> {
    if !args.method.eq_ignore_ascii_case("GET") {
        return Err(CliError::Usage(format!(
            "athena is read-only: `api` takes GET, not {}",
            args.method
        )));
    }
    if args.path.trim().is_empty() {
        return Err(CliError::Usage("api needs a path".into()));
    }
    let path = args.path.trim();
    let opened = open_session(ctx, &args.select)?;
    // A URL elsewhere is the caller's mistake, refused before the token
    // could travel: exit 2.
    if path.contains("://") && !crate::http::same_origin(path, &opened.entry.fhir_base) {
        return Err(CliError::Usage(
            "that URL is not on the signed-in FHIR server; athena sends its token nowhere else"
                .into(),
        ));
    }
    let client = start_client(ctx, &opened)?;
    let result = client.get_raw(path.trim_start_matches('/'));
    note_renewal(ctx, &opened.entry, &opened.stored, &client.session());
    let body = result?;
    match serde_json::from_str::<Value>(&body) {
        Ok(value) => output::json(&value),
        Err(_) => println!("{body}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_stored_sign_in_shows_its_client_id_by_its_last_four_characters() {
        let now = 1_800_000_000;
        let mut stored = session::test_session("clinic", "p1", None, now);
        stored.client_id = "0oaEXAMPLEclient9876".into();
        let mut row = Map::new();
        assert!(stored_columns(&mut row, &stored, now));
        assert_eq!(row["client_id_ends_with"], "9876");
        assert!(!Value::Object(row).to_string().contains("0oaEXAMPLE"));
    }

    use super::*;

    #[test]
    fn a_refused_refresh_moves_the_index_horizon_with_the_keychain() {
        use crate::env::Environment;
        let dir = std::env::temp_dir().join(format!("athena-note-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = Ctx {
            json: true,
            quiet: true,
            interactive: false,
            store: ConfigStore::new(BIN).with_override(Some(dir.join("config.json"))),
            profile: Some("adult1".into()),
            vault: OnceCell::new(),
        };
        let now = unix_secs();
        let mut before = session::test_session("mock", "a-900001.E-1001", None, now);
        before.tokens.refresh_token = Some("refresh-1".into());
        let entry = SessionEntry {
            profile: Some("adult1".into()),
            org: "mock".into(),
            patient: "a-900001.E-1001".into(),
            label: None,
            relationship: None,
            environment: Environment::Custom,
            fhir_base: before.fhir_base.clone(),
            stored_at: rfc3339(now),
            signin_expires_at: Some(rfc3339(session::signin_expires_at(&before))),
            login: None,
        };
        let mut index = SessionIndex::default();
        index.upsert(entry.clone());
        index.save(&ctx.sessions_path().unwrap()).unwrap();
        // What the renewer stores after an invalid_grant: the same tokens,
        // stamped refused.
        let mut refused = before.clone();
        refused.refused_at = Some(now + 7);
        note_renewal(&ctx, &entry, &before, &refused);
        let saved = SessionIndex::load(&ctx.sessions_path().unwrap()).unwrap();
        assert_eq!(
            saved.sessions[0].signin_expires_at.as_deref(),
            Some(rfc3339(now + 7).as_str())
        );
        assert!(!session::usable(&refused, now));
        // Nothing answered: the index is left alone.
        let unchanged = before.clone();
        assert!(!renewed(&before, &unchanged));

        // A relabel while the command ran stands, and a sign-out is not undone.
        let mut index = SessionIndex::load(&ctx.sessions_path().unwrap()).unwrap();
        index.sessions[0].label = Some("adult1".into());
        index.save(&ctx.sessions_path().unwrap()).unwrap();
        note_renewal(&ctx, &entry, &before, &refused);
        let saved = SessionIndex::load(&ctx.sessions_path().unwrap()).unwrap();
        assert_eq!(saved.sessions[0].label.as_deref(), Some("adult1"));
        SessionIndex::default()
            .save(&ctx.sessions_path().unwrap())
            .unwrap();
        note_renewal(&ctx, &entry, &before, &refused);
        let saved = SessionIndex::load(&ctx.sessions_path().unwrap()).unwrap();
        assert!(saved.sessions.is_empty());
    }

    #[test]
    fn only_synthetic_servers_may_be_written_to_a_plain_file() {
        for base in [
            "http://127.0.0.1:4100/fhir/r4",
            "http://localhost:4100/x/fhir/r4",
            "https://api.preview.platform.athenahealth.com/fhir/r4",
            "https://api.preview.platform.athenahealth.com/fhir/r4/",
            "https://api.preview.platform.athenahealth.com/80000/brand/1/csg/1/fhir/r4",
            "https://api.preview.platform.athenahealth.com/195900/brand/1/csg/1/fhir/r4",
        ] {
            assert!(synthetic(base), "{base}");
        }
        for base in [
            "https://api.platform.athenahealth.com/fhir/r4",
            "https://api.platform.athenahealth.com/80000/brand/1/csg/1/fhir/r4",
            "https://api.preview.platform.athenahealth.com/900001/brand/1/csg/1/fhir/r4",
            "https://api.preview.platform.athenahealth.com/800001/fhir/r4",
            "https://api.preview.platform.athenahealth.com.example/fhir/r4",
            "https://127.0.0.1.example/fhir/r4",
            "http://127.0.0.1.example:80/fhir/r4",
            "not a url",
        ] {
            assert!(!synthetic(base), "{base}");
        }
    }

    /// A context over a temp directory, its vault keyed in memory.
    fn test_ctx(dir: &Path) -> Ctx {
        let ctx = Ctx {
            json: true,
            quiet: true,
            interactive: false,
            store: ConfigStore::new(BIN).with_override(Some(dir.join("config.json"))),
            profile: None,
            vault: OnceCell::new(),
        };
        let vault = Vault::new(pk_cli_auth::vault::MemoryKeys::new(), dir.join("vault"));
        let _ = ctx.vault.set(Arc::new(vault));
        ctx
    }

    fn old_entry(patient: &str, label: &str) -> SessionEntry {
        SessionEntry {
            profile: None,
            org: "wide".into(),
            patient: patient.into(),
            label: Some(label.into()),
            relationship: Some("self".into()),
            environment: Environment::Production,
            fhir_base: "https://api.platform.athenahealth.com/fhir/r4".into(),
            stored_at: "2026-01-01T00:00:00Z".into(),
            signin_expires_at: None,
            login: None,
        }
    }

    /// A session whose ID token names login `sub`, as a v0.3 keychain item.
    fn old_item(keys: &dyn KeyStore, patient: &str, sub: &str) {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut s = session::test_session("wide", patient, None, 1_000);
        s.tokens.id_token = Some(format!(
            "{}.{}.sig",
            b64.encode(br#"{"alg":"RS256"}"#),
            b64.encode(json!({"iss": "https://idp.example.test", "sub": sub}).to_string())
        ));
        let text = serde_json::to_string(&s).unwrap();
        keys.set(
            &session::key("wide", patient),
            &pk_cli_secrets::Secret::new(text),
        )
        .unwrap();
    }

    /// Revocations, answered 200 and counted.
    #[derive(Default)]
    struct Revocations(std::sync::Mutex<Vec<String>>);

    impl crate::http::Http for Revocations {
        fn call(
            &self,
            request: pk_cli_core::transport::HttpRequest,
        ) -> Result<pk_cli_core::transport::HttpResponse, CliError> {
            self.0.lock().unwrap().push(request.url.clone());
            Ok(pk_cli_core::transport::HttpResponse::new(200, ""))
        }
    }

    fn migrate(
        ctx: &Ctx,
        keys: &dyn KeyStore,
        name: &str,
        patients: &[&str],
    ) -> Result<(), CliError> {
        migrate_revoking(ctx, keys, name, patients, &Revocations::default())
    }

    fn migrate_revoking(
        ctx: &Ctx,
        keys: &dyn KeyStore,
        name: &str,
        patients: &[&str],
        http: &Revocations,
    ) -> Result<(), CliError> {
        let lock = RefreshLock::at(ctx.state_dir().unwrap().join("refresh.lock"));
        migrate_with(
            ctx,
            MigrateCmd {
                name: name.into(),
                org: "wide".into(),
                patients: patients.iter().map(|p| p.to_string()).collect(),
            },
            keys,
            &lock,
            http,
        )
    }

    #[test]
    fn migrate_revokes_the_copy_it_does_not_keep() {
        use pk_cli_auth::vault::MemoryKeys;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let keys = MemoryKeys::new();
        let mut index = SessionIndex::default();
        index.upsert(old_entry("a-1.E-1", "newborn"));
        index.save(&ctx.sessions_path().unwrap()).unwrap();
        // The profile signed in again after the upgrade: an older copy in the
        // vault, and the v0.3 item, newer, with other tokens.
        let profile = ProfileName::parse("adult1").unwrap();
        let mut config = ctx.load().unwrap();
        config.profiles.add(profile.clone());
        ctx.save(&config).unwrap();
        let revoke = Some("https://idp.example.test/revoke");
        let mut held = session::test_session("wide", "a-1.E-1", revoke, 500);
        held.tokens.access_token = "access-held".into();
        ctx.sessions(&profile).unwrap().save(&held).unwrap();
        let mut newer = session::test_session("wide", "a-1.E-1", revoke, 900);
        newer.tokens.access_token = "access-newer".into();
        keys.set(
            &session::key("wide", "a-1.E-1"),
            &pk_cli_secrets::Secret::new(serde_json::to_string(&newer).unwrap()),
        )
        .unwrap();

        let http = Revocations::default();
        migrate_revoking(&ctx, &keys, "adult1", &["newborn"], &http).unwrap();
        // The newer copy is kept and the dropped one's access token revoked
        // (it has no refresh token).
        let store = ctx.sessions(&profile).unwrap();
        let kept = store
            .load(&session::key("wide", "a-1.E-1"))
            .unwrap()
            .unwrap();
        assert_eq!(kept.tokens.access_token, "access-newer");
        assert_eq!(http.0.lock().unwrap().len(), 1);
        assert!(keys
            .get(&session::key("wide", "a-1.E-1"))
            .unwrap()
            .is_none());

        // Two copies of one grant (a renewal changed only the access token):
        // nothing is revoked, so the copy kept stays live.
        let mut index = ctx.index().unwrap();
        index.upsert(old_entry("a-1.E-2", "adult1"));
        index.save(&ctx.sessions_path().unwrap()).unwrap();
        let mut renewed = session::test_session("wide", "a-1.E-2", revoke, 900);
        renewed.tokens.refresh_token = Some("refresh-shared".into());
        renewed.tokens.access_token = "access-renewed".into();
        store.save(&renewed).unwrap();
        let mut stale = renewed.clone();
        stale.tokens.access_token = "access-stale".into();
        stale.tokens.obtained_at = 500;
        keys.set(
            &session::key("wide", "a-1.E-2"),
            &pk_cli_secrets::Secret::new(serde_json::to_string(&stale).unwrap()),
        )
        .unwrap();
        let http = Revocations::default();
        migrate_revoking(&ctx, &keys, "adult1", &["adult1"], &http).unwrap();
        assert!(http.0.lock().unwrap().is_empty());
        let kept = store
            .load(&session::key("wide", "a-1.E-2"))
            .unwrap()
            .unwrap();
        assert_eq!(kept.tokens.access_token, "access-renewed");
    }
    #[test]
    fn migrate_moves_the_named_sign_ins_into_the_profile_and_only_them() {
        use pk_cli_auth::vault::MemoryKeys;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let keys = MemoryKeys::new();
        let mut index = SessionIndex::default();
        for (patient, label, sub) in [
            ("a-1.E-1", "newborn", "login-a"),
            ("a-1.E-2", "adult1", "login-a"),
            ("a-1.E-3", "adult2", "login-b"),
        ] {
            index.upsert(old_entry(patient, label));
            old_item(&keys, patient, sub);
        }
        index.upsert(old_entry("a-1.E-9", "gone"));
        index.save(&ctx.sessions_path().unwrap()).unwrap();

        migrate(&ctx, &keys, "adult1", &["newborn", "a-1.E-2", "gone"]).unwrap();
        let config = ctx.load().unwrap();
        assert!(config
            .profiles
            .contains(&ProfileName::parse("adult1").unwrap()));
        let index = ctx.index().unwrap();
        let moved = index.matching(Some("adult1"), None, None);
        assert_eq!(moved.len(), 2);
        assert!(moved[0].login.is_some() && moved[0].login == moved[1].login);
        // Nothing was stored for `gone`: it leaves the index.
        assert!(index.matching(None, Some("gone"), None).is_empty());
        assert_eq!(index.matching(None, None, None).len(), 1);
        assert!(keys
            .get(&session::key("wide", "a-1.E-1"))
            .unwrap()
            .is_none());
        assert!(keys
            .get(&session::key("wide", "a-1.E-3"))
            .unwrap()
            .is_some());
        let vault = ctx.vault().unwrap();
        assert_eq!(
            vault.names().unwrap(),
            vec![
                "adult1/wide/a-1.E-1".to_string(),
                "adult1/wide/a-1.E-2".to_string()
            ]
        );
        let store = ctx
            .sessions(&ProfileName::parse("adult1").unwrap())
            .unwrap();
        assert!(session::load(&*store, moved[0]).is_ok());

        // A second run finds them moved and changes nothing.
        migrate(&ctx, &keys, "adult1", &["newborn", "adult1"]).unwrap();
        assert_eq!(ctx.index().unwrap(), index);

        // Another login's sign-in into the same profile: moved, and counted.
        migrate(&ctx, &keys, "adult1", &["adult2"]).unwrap();
        let index = ctx.index().unwrap();
        assert_eq!(
            distinct_logins(&index, &ProfileName::parse("adult1").unwrap()),
            2
        );
        let entry = index.matching(Some("adult1"), Some("adult2"), None)[0].clone();
        assert_eq!(login_matches_profile(&index, &entry), Some(false));
    }

    #[test]
    fn migrate_refuses_a_label_the_profile_has_and_an_unknown_sign_in() {
        use pk_cli_auth::vault::MemoryKeys;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let keys = MemoryKeys::new();
        let mut index = SessionIndex::default();
        index.upsert(old_entry("a-1.E-1", "newborn"));
        let mut taken = old_entry("a-1.E-5", "newborn");
        taken.profile = Some("adult2".into());
        index.upsert(taken);
        index.save(&ctx.sessions_path().unwrap()).unwrap();
        old_item(&keys, "a-1.E-1", "login-b");

        let err = migrate(&ctx, &keys, "adult2", &["newborn"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.remedy().unwrap().contains("auth relabel"), "{err}");
        let err = migrate(&ctx, &keys, "adult2", &["nobody"]).unwrap_err();
        assert_eq!(err.exit_code(), 4);
        let err = migrate(&ctx, &keys, "Adult2", &["newborn"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        // Nothing moved and no profile was added.
        assert!(keys
            .get(&session::key("wide", "a-1.E-1"))
            .unwrap()
            .is_some());
        assert!(ctx.load().unwrap().profiles.is_empty());
    }

    #[test]
    fn the_command_tree_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn labels_are_checked() {
        assert!(validate_label("--label", "kid1").is_ok());
        assert!(validate_label("--label", "kid_2").is_ok());
        assert!(validate_label("--label", "two words").is_err());
        assert!(validate_label("--label", "").is_err());
    }
}
