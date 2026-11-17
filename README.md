# athena

Pull your own health records, and your children's through caregiver access,
from medical practices that run athenahealth's athenaOne, through
athenahealth's patient-access SMART on FHIR API.

Unofficial; not affiliated with or endorsed by athenahealth, Inc.
athenahealth and athenaOne are athenahealth's trademarks.

```sh
athena orgs refresh                                    # athenahealth's directory of practices, once
athena orgs search "<practice name>" "<city>"
athena config set practice clinic --env production --practice-id <P> --brand-id <B> --csg-id <C>
athena profiles add adult1                             # one profile per person who signs in
export ATHENA_PROFILE=adult1                           # or --profile adult1 on each command
athena auth login --org clinic --label kid1            # sign in in adult1's browser window, one record at a time
athena patients list
athena records export --patient kid1 --ndjson | medrec import --from athena --profile adult1
athena documents list --patient kid1
athena documents download DocumentReference/<id> -o note.pdf --patient kid1
athena api GET Patient/<id> --patient kid1
```

`athena` only reads. Records go where you send them; nothing is sent
anywhere else.

## Before the first sign-in

Each household registers its own free patient app with athenahealth and gives
`athena` the two client ids: [docs/registration.md](docs/registration.md).

```sh
athena config set client-id <PREVIEW_CLIENT_ID> --env preview
athena config set client-id <PRODUCTION_CLIENT_ID> --env production
```

## Install

```sh
curl -sSL https://github.com/piekstra/athena-cli/releases/latest/download/athena-aarch64-apple-darwin.tar.gz | tar xz
mv athena ~/.local/bin/        # or anywhere on PATH
athena self-update             # later releases
```

Release assets: `athena-<target>.tar.gz` and `.sha256` for
`aarch64-apple-darwin`, `x86_64-apple-darwin` and
`x86_64-unknown-linux-gnu`. From source: `make install`.

## Commands

| Command | What it does |
|---|---|
| `orgs refresh\|list\|search` | Practice profiles (`org-list/v1`), and athenahealth's directory of practices, downloaded on `refresh` (about 135 MB) and kept as names, places and ids. A search match's `id` is the directory's `<practice>-<brand>-<csg>` key; `--org` takes a profile's slug |
| `config set\|unset practice <slug>` | A practice profile: environment, practice, brand and chart sharing group ids, or a FHIR base in full |
| `profiles list\|add\|remove` | Login profiles, one per person who signs in (`profile-list/v1`). `remove` signs out the profile's sign-ins, revoking them, and removes its browser profile; it asks first, or takes `--force` |
| `profiles migrate <NAME> --org <ORG> --patient <ID\|LABEL>...` | Move sign-ins stored before v0.4.0 into a profile (`profile-migrate/v1`); `profiles list` shows them |
| `auth login --org <ORG> [--label <WHO>]` | Standalone patient launch in the profile's own Chrome window (PKCE, loopback); one sign-in per patient record. It refuses a grant with no read scope and names the types a partial grant leaves out (`types_not_granted`). A second sign-in for the same patient replaces the first and revokes its tokens |
| `auth status` / `auth logout` | Each sign-in's state (`auth-status/v1`); remove one or all |
| `auth relabel --org <ORG> --from <LABEL> --to <LABEL>` | Rename a sign-in's label (`auth-relabel/v1`), in the local index only: no keychain, no network |
| `patients list [--org <ORG>]` | The patients signed in, each with `self` or `proxy` (`patient-list/v1`) |
| `records export --patient <ID> [--since D] --ndjson` | Every resource the practice serves to the patient, files included, as `records-stream/v1` |
| `documents list\|download` | Visit notes, reports and results' forms (`documents/v1`) |
| `api GET <path>` | One FHIR read as the signed-in patient |
| `config path\|show\|set\|unset` | Client ids, redirect ports, the scope version (`v2`, or `v1` for an app registered with SMART V1 scopes only), practice profiles |
| `info`, `completions`, `self-update` | Family standard |

Every command takes `--json`. Every command that uses a sign-in (`auth`,
`patients`, `records`, `documents`, `api`) takes `--profile <NAME>`, or
`ATHENA_PROFILE`; there is no default, and a missing or unknown profile is
exit 2. Exit codes: 0 ok, 2 usage, 3 sign-in needed or refused, 4 not
found, 5 the practice or network failed, 6 confirmation needed.

`records export -o <DIR>` writes `<DIR>/records.ndjson` instead and prints
`records-export/v1`, for athenahealth's sandbox and local test servers only.
The file is owner-only and written beside `records.ndjson` until the
trailer is out. A complete pull replaces `records.ndjson`. A pull cut short
goes to `records.incomplete.ndjson` (`complete: false`), keeps the last
complete export, and exits with its error's code. v0.1.0 wrote a new
`<profile>_<patient>_<time>.ndjson` on each run, and v0.2.0 replaced
`records.ndjson` even when a run failed.

## Profiles

A profile is one person's athenahealth login: `adult1`, `adult2`. Its
sign-ins, its labels and its browser window are its own, so one parent's
commands never run as the other's. Name them for the person, never with a
name from a record; they stay in the local config.

Each profile signs in in its own Chrome profile, so one login's cookies never
sign in another, and Chrome remembers each login's device separately. Chrome
must be installed. With `--no-browser`, or when the window cannot open,
`athena` prints the address instead: open it in a private window, since a
browser where another login is signed in to athenahealth signs in as that
login. A sign-in from another login than the profile's other sign-ins is
stored with a warning (`login_matches_profile: false`).

## How sign-in works

1. `athena` reads athenahealth's sign-in endpoints and opens a page on
   `127.0.0.1` with a "Log in with athenahealth" button, in the profile's
   Chrome window.
2. You sign in with your patient portal account. A caregiver picks one
   record there; each record is one sign-in (`--label` names it).
3. athenahealth redirects to `http://127.0.0.1:47816/callback` (or `:47817`),
   where `athena` listens, and `athena` exchanges the code with PKCE.
4. Access tokens last an hour and renew themselves; a sign-in lasts 90 days
   from its last use. `auth status` shows when each one ends, and when
   athenahealth last refused one (`refused_at`).

Tokens are kept in athena's vault: each sign-in in its own encrypted,
owner-only file under `~/.config/athena/vault/`, named
`<profile>/<practice profile>/<patient>` inside, under one macOS Keychain
item, the vault's key (`piekstra.athena`, account `vault-key`). A new or
upgraded `athena` therefore asks the Keychain once, however many sign-ins
there are. Nothing secret is written anywhere else.

Up to v0.3.x each sign-in was its own Keychain item (account
`smart/<practice profile>/<patient>`) in no profile. v0.4.0 does not read
them until `athena profiles migrate <NAME> --org <ORG> --patient <ID|LABEL>`
moves them, one person's at a time: each item costs one Keychain prompt,
once, and is deleted once its copy in the vault reads back. Delete test
sign-ins rather than moving them. v0.3.0 renamed the account from
`<practice profile>/<patient>`, so a sign-in made with v0.2.x is not found;
`auth logout` removes such an item too, and when it cannot be read (its
Keychain prompt was denied) the patient stays listed so `auth logout` can be
run again, or `security delete-generic-password -s piekstra.athena -a
<practice profile>/<patient>` removes it by hand.

## What a patient token sees

What the practice has published to its patient portal: athenahealth filters
out unpublished notes and results for patient sign-ins. There is no imaging
study download; imaging reports come as documents. Details, with
athenahealth's wording: [docs/api.md](docs/api.md).

## Docs

- [docs/registration.md](docs/registration.md): registering a household's app
- [docs/api.md](docs/api.md): athenahealth's behaviour and traps, with its wording
- [AGENTS.md](AGENTS.md): working on the code

Licensed under MIT or Apache-2.0.
