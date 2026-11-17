# athenahealth's patient-access API, as athena uses it

What athena relies on, in athenahealth's own words where it has them, and the
traps found on the way. Quotations are from athenahealth's developer
documentation at <https://docs.athenahealth.com/api/>, read on 2026-11-12;
the page each comes from is named beside it. Everything marked **unverified**
waits on a sign-in against athenahealth's Preview sandbox.

## Route: Certified APIs, as a PHR app

athenahealth publishes two families of API: its proprietary athenaOne APIs
(contract required) and its **Certified APIs**, the ONC (g)(10) patient-access
API that the 21st Century Cures Act requires. athena uses only the second.

- "athenahealth offers access to our Certified APIs free of charge to the
  healthcare developer community." ([23.3 release note][r233])
- A patient-facing app that reads its user's own records is a **PHR app**:
  "If you are developing a PHR app that exclusively uses Certified APIs ...
  please follow the self-service onboarding process ... You must use
  3-legged OAuth. Upon app creation, your application will automatically be
  enabled to access all athenahealth tablespaces. Your app will not be able to
  retrieve a patient's data, however, until a patient consents to the app's
  access request." ([Certified APIs][certified])
- A practice can switch it off: "Individual athenaOne customers will also be
  able to disable your app from accessing their patients' data via the Manage
  PHR Access admin page." ([Certified APIs][certified])
- The app is listed under a name that follows athenahealth's
  [App Naming Guidelines][naming]: it must "Include your company name/brand"
  and cannot contain "athenahealth branded words such as "athena",
  "athenahealth"", environment words ("prod", "preview", "dev"), "Usernames,
  email addresses, website URLs, names of people/teams", "app",
  "application", "The words "CAPI" or "PHR"", or, for Certified-API-only
  apps, "Test", "Temp" and the like.

**Portal automation is not a route.** The [Developer Portal Terms and
Conditions][terms] forbid it outright: you may not "apply systems to extract
or modify information from the API or athenaOne using technology or methods
such as those commonly referred to as "web scraping," "data scraping," or
"screen scraping"". athena reads the API only.

### The terms a registration agrees to

[Developer Portal Terms and Conditions][terms], the points that bind a
household tool:

| Section | athenahealth's wording, shortened |
|---|---|
| License | "a limited, non-exclusive, revocable, non-transferable, and non-sublicensable right to access and use the Developer Portal, API and ... Materials" |
| Restrictions | no "reverse engineer, reverse compile, disassemble the API or Materials"; no scraping (above); no redistributing "access to the API"; no use "for purposes of monitoring the availability, performance, or functionality of our services or for any other benchmarking purposes" |
| Keys | "You will be issued one or more unique passwords, security codes, or tokens (each a "Key")"; Keys are not shared, and "You may not transfer, assign, or delegate these terms or your Keys to any third party" |
| Fees | only "In the event that you have an agreement with Athena"; Certified APIs are free (above) |
| Termination | "Athena may change, suspend, or discontinue the API and suspend or terminate your use ... at any time for any reason, with or without notice" |
| Indemnity | the developer indemnifies athenahealth for use "in violation of these Terms" |
| Liability | "Under no circumstances shall Athena be liable to you for any direct damages in excess of $100" |
| Law | "the laws of the Commonwealth of Massachusetts" |

So athena keeps the client id in local configuration, never in the
repository: each household registers its own app (`docs/registration.md`).

## Environments

| | Preview (sandbox) | Production |
|---|---|---|
| API host | `https://api.preview.platform.athenahealth.com` | `https://api.platform.athenahealth.com` |
| OAuth | `<host>/oauth2/v1/{authorize,token,revoke,introspect,keys,logout}` | same |
| FHIR R4, athenahealth-wide | `<host>/fhir/r4` | same |
| FHIR R4, one practice | `<host>/{practiceid}/brand/{brandid}/csg/{chartsharinggroupid}/fhir/r4` | same |
| Okta issuer (SMART configuration) | `https://athena.okta.com/oauth2/aus2hfei6ookPyyCA297` | `https://athena.okta.com/oauth2/aus2hff5eqFb7Wqfh297` |

A client id is issued per environment: the Developer Console makes Preview and
Production credentials separately ([Onboarding Overview][onboarding]).

"These site-specific URLs require the appropriate practice ID(s), brand ID(s),
and chart sharing group ID(s) for that organization." ([FHIR Base URLs][base])

## Sign-in: SMART standalone patient launch

athena signs in through medical-common's `pk_med_smart` (v0.5.0):
`discover` reads `.well-known/smart-configuration` (and checks that an https
server names only https endpoints), `begin` binds the redirect listener and
builds the authorize URL with PKCE S256, state, `aud` and a nonce, and
`SmartSignIn::exchange` trades the code and checks the nonce. athena adds
athenahealth's start page and keeps its revocation endpoint
(`src/login.rs`). `src/smart.rs` records why `Athenahealth`'s
`VendorQuirks` are the defaults.

### Public client and PKCE

athena is a native app with no secret. "PKCE is required by the token API for
native or single-page browser apps that cannot securely store a client
secret." ([Token Endpoint][token]) The SMART configuration lists
`client-public`, `context-standalone-patient`, `permission-offline`,
`permission-v1`, `permission-v2`, `token_endpoint_auth_methods_supported`
including `none`, and `code_challenge_methods_supported: ["S256"]`.

### Redirect URI: exact match, so fixed loopback ports

"The redirect URI must exactly match a post-login redirect URI configured for
your app's OAuth client credentials during registration." ([Authorize
Endpoint][authorize]) Okta compares the whole URI, port included, so athena
does not use an ephemeral port: it listens on 127.0.0.1 at
`http://127.0.0.1:47816/callback`, or `:47817` when that is taken, and the
registration lists both. `athena config set redirect-ports` changes them,
and the registration must change with them. `pk_med_smart` can instead send
a registered https page that forwards to the loopback
(`SmartApp::redirect_uri`, v0.5.0); athena does not use it, because its
registration lists the loopback URLs themselves.

`auth login` keeps the loopback open for 30 minutes by default (`--timeout`,
1 to 60), from v0.3.10; it was 5. On 2026-11-17 a caregiver's sign-in took
more than 15 minutes, and its code arrived after the listener had closed. A
code cannot be exchanged by a later process: its PKCE verifier lived only in
the one that started the sign-in.

### `aud` chooses the practice, or none

"The value is the FHIR base URL, used to specify the organization for which
the end user will be authenticated. The FHIR base URL may specify practice
ID, department ID, brand ID, chart sharing group ID, or none of the above to
indicate a global login." ([Authorize Endpoint][authorize])

For the athenahealth-wide base: "If cross-context authentication is used with
SMART on FHIR, in which patients must select a single patient record to grant
access, patients will encounter an updated prompt that indicates the
athenahealth Patient Portal brand associated with each of their records."
([22.7 release note][r227])

A practice profile with practice, brand and chart sharing group ids signs in
against that practice's base; one without them (the built-in `sandbox`) signs
in athenahealth-wide.

### One patient record per sign-in

"patients must select a single patient record to grant access" (above). A
caregiver who sees several children in the portal signs in once per child:
each sign-in yields one token set for one `Patient`, kept as its own keychain
item. The token answer names the patient (`"patient": "a-195900.E-60178"` in
athenahealth's shape: `a-<practiceid>.E-<chart id>`). athenahealth's API
gateway adds that member (seen 2026-11-17 for the sandbox's test patient):
the Okta server behind it answers without one, and carries the patient only
as access-token claims (`patient`, `selectedPatient`, `launch_context`),
which athena does not read.

An answer that names no patient is refused. athena revokes its tokens,
stores nothing, and exits 5 with two facts that tell the causes apart:
whether `launch/patient` was granted, and the resource type the id token's
`fhirUser` names (`Patient`, `RelatedPerson` or none). On 2026-11-17 a
caregiver's production sign-in, against a practice's own base, got such an
answer after a timed-out login page was retried. v0.3.8 and earlier left
those tokens live.

Whether the sign-in was the patient's own or a caregiver's is read from the id
token's `fhirUser`: the patient's own `Patient/<id>` is `self`, anything else
(a `RelatedPerson`) is `proxy`. The sandbox test patient's own sign-in reads
as `self` (2026-11-17). So did a caregiver's production sign-in for a
child's record that day: athenahealth's `fhirUser` named the chosen
`Patient`. `relationship` therefore cannot tell a caregiver's sign-in from
the patient's own, and the label given at sign-in is what says whose record
it is.

A caregiver's production sign-in against the athenahealth-wide base listed
records at more than one practice in the record picker, and the token answer
named the patient. Against the practice's own base, one sign-in that day got
an answer without a patient (above).

The choice is made on athenahealth's side, after its login: a page on
`myidentity-support.platform.athenahealth.com` reads "Select a health record
... Your account is linked to the following health records", listing the
account's own record marked "(you)" and, for a caregiver, the records it may
open for others. It can add "Could not confirm access to additional health
records. Please check again later." (seen 2026-11-16 for the sandbox's test
patient). Choosing a record continues to the consent page.

### Scopes

- "athenahealth automatically approves all 3-legged OAuth apps for the
  following scopes ...: openid, fhirUser, offline_access" ([Authorization
  Overview][authz]), and, for "patient-facing apps using 3-legged OAuth ... for
  use with the standalone SMART App Launch sequence", `launch/patient` and
  `patient/<Type>.read` for each listed type.
- The Developer Portal's scope editor (2026-11-16) has products athenaOne,
  Event Notifications, FHIR DSTU2, FHIR R4 SMART V1 and FHIR R4 SMART V2.
  SMART V1 offers `patient/<Type>.read` for 21 types, without Coverage,
  Media, MedicationDispense, RelatedPerson and Specimen. SMART V2 offers `.r`
  and `.s` for all 26 of `PATIENT_TYPES`, and granting them adds `.rs` and
  category-limited Condition and Observation scopes. The OpenID scopes and
  `launch/patient` are not in the editor. The SMART configuration lists both
  `permission-v1` and `permission-v2`.
- athena asks for `patient/<Type>.rs` (SMART v2), the spelling the editor
  grants for every type it reads; `athena config set scope-version v1` asks
  `patient/<Type>.read` instead, for an app registered with SMART V1 scopes
  only. `granted_types` reads v1 and v2 spellings, category narrowings
  included. The sandbox granted a v2 request in full on 2026-11-17: 39
  scopes, every type in `PATIENT_TYPES` (`types_not_granted` empty). The
  consent page ("<app name> is asking to access parts of
  your medical record") listed 37 permissions on 2026-11-16, all ticked, the
  five SMART-v2-only types among them and "Have offline access" last, under
  one "Allow Access" button. What is granted is read from the token answer.
- After the code exchange, athena compares the token answer's `scope` with
  what it asked for. An answer that grants no read scope is refused (exit 3):
  the sign-in is not stored, its tokens are revoked, and the remedy names the
  registration step and `scope-version`. A partial grant is stored with a
  warning, and `auth login --json` lists the rest as `types_not_granted`;
  each export reports them as `forbidden` skips. A token answer without
  `scope` grants what was asked (RFC 6749 §5.1).
- athenahealth's authorize endpoint judges scopes only after the sign-in.
  An unauthenticated `GET /oauth2/v1/authorize` (2026-11-16) reached its
  Login page for every scope set tried: v1 for 26 types, v1 for 21, v2 for
  26, the base four alone, and a set with a scope that does not exist. A
  scope the app was not granted can therefore show only after the person
  signs in.
- "By default, athenahealth does not permit apps to request wildcard scopes
  ... nor does athenahealth permit write scopes" ([Authorization
  Overview][authz]). athena names every type: `src/smart.rs`, `PATIENT_TYPES`.
- "To obtain a refresh token in the token endpoint response, you must request
  the offline access scope." ([Authorize Endpoint][authorize])
- `nonce` is "Required if requesting the openid scope" ([Authorize
  Endpoint][authorize]). `pk_med_smart::begin` sends a fresh one whenever
  `openid` is asked for, and `exchange` refuses an ID token that carries
  another (exit 3, `CredentialRefused`). An answer with no ID token at all
  is not refused.

athena asks for the R4 types it reads. The auto-approved list also names
DSTU2-era and proprietary types (`Assessment`, `ClinicalImpression`,
`MedicalAdministration`, `MedicationOrder`, `MedicationStatement`,
`PracticeConfiguration`, `Provider`) that the R4 CapabilityStatement does not
serve, and `Appointment`, whose R4 search takes no `patient` parameter.

### The "Log in with athenahealth" button

athenahealth asks patient-facing apps to start sign-in from its "Log in with
athenahealth" button (CSS and logo from
`identity-static-assets.platform.athenahealth.com/login-with-athenahealth/latest/`).
Its sample script builds a production-only authorize URL with neither `aud`
nor PKCE, so athena does not use the script: it serves its own start page
with the button's markup and classes, linking to the full authorize URL. The
page is on a second 127.0.0.1 listener at a port the system picks, because
cli-common's redirect listener (on 47816 or 47817) answers only `/callback`.
It answers `GET /` with its own `Host` only, and stops when the sign-in ends.
That listener's last page is cli-common's ("signed in", or "the sign-in did
not complete"), and a cancelled sign-in is exit 3 with "the provider refused
the sign-in (access_denied…)". The listener sends that page when the code
arrives, before athena exchanges it, so "signed in" means athenahealth sent a
code back, not that a sign-in was stored. The terminal's exit code and
`auth status` say whether it was.

## Tokens

From the [Token Endpoint][token] page:

- "expires_in - represents the lifetime of the access token in seconds
  (default is 60 minutes ...)"
- "refresh_token – JWT used to obtain a new access token without requiring
  re-authentication by an end user (3-legged OAuth only; default expiration
  is 90 days from the most recent token request that used or generated that
  refresh token)"
- `scope` on a refresh: "Required if grant_type is ... refresh_token (3-legged
  OAuth only) ... If grant_type is refresh_token, the requested scopes must be
  a subset of those provided to the authorize endpoint when the refresh token
  was originally generated." athena sends the whole granted set.
- `redirect_uri` on the code exchange "must match the value for redirect_uri
  provided to the authorize endpoint".

A sign-in is one `pk_med_smart::SmartSession` in the Keychain, account
`smart/<profile>/<patient>`. It keeps a refresh token until a token answer
carries a new one, and `tokens.obtained_at` is the last grant, so athena
reports the 90-day horizon as `signin_expires_at` from it. `pk_med_smart::Renewer` runs
every renewal under the family's refresh lock (`pk_cli_auth::refresh_lock`),
so two processes never present one refresh token at once. A token that has
lapsed is renewed before the first request; one athenahealth refuses (401)
is renewed once and the request sent again. A refused refresh grant stamps
`refused_at` on the stored session and keeps the refresh token, so `auth
status` reports the sign-in unusable (`refused_at`, and `signin_expires_at`
then), and the next command tries one grant before it asks for a new
sign-in. v0.2.x dropped the refresh token instead. On 2026-11-17 the
sandbox's tokens were renewed at the gateway's token endpoint twice: once
after the RelatedPerson 401, and once on a token about to lapse. Each
renewed access token read and searched like the first, for an hour.
**Unverified**: whether a refresh answer carries a new refresh token.

Revocation is `POST /oauth2/v1/revoke` (RFC 7009); `auth logout` revokes the
refresh and access tokens, then deletes the keychain item. The logout
endpoint "Terminates an end user's ... login session in the athenahealth login
widget, which otherwise expires after 10 minutes of idle time ... terminating
a login session will not invalidate any active access or refresh tokens"
([Additional OAuth Endpoints][oauth-extra]); athena has no use for it.

v0.2.x stored a sign-in at `<profile>/<patient>`, which nothing else reads
since v0.3.0, so `auth logout` revokes and deletes that item too (one that
does not read as a sign-in is deleted unrevoked). Each row of
`auth-logout/v1` has `credential_removed` and `revoked`, which is true only
when every revocation tried for the patient's items succeeded. A patient whose
v0.2.x item cannot be read or deleted (a denied Keychain prompt) is listed
under `incomplete` with an `error`, not under `signed_out`. It stays in the
index so a second `auth logout` retries it. The warning names the manual
`security delete-generic-password` step, and the command exits with the
error's code (1) after its one document.

## FHIR R4

The CapabilityStatement (`/fhir/r4/metadata`) is FHIR 4.0.1, instantiating US
Core 3.1.1 and 6.1.0 and Bulk Data. Searches page with an opaque `cursor`
parameter in `Bundle.link[next]`.

athena exports, by `patient=<id>` search (all take `patient`, `_lastUpdated`
for `--since`): AllergyIntolerance, Condition, MedicationRequest,
MedicationDispense, Immunization, Encounter, Procedure, Observation,
DiagnosticReport, DocumentReference, ServiceRequest, Specimen, CarePlan,
CareTeam, Goal, Device, Coverage, RelatedPerson; then `Patient/<id>` by read,
and Practitioner, Organization, Location, Medication and Media by read when an
exported resource refers to them.

On 2026-11-17 the sandbox served none of those referenced resources to its
test patient's token, although the grant named all four types. Each read
answered 404 `not-found` ("A resource with that id does not exist"), for ids
such as `Practitioner/a-80000.Provider-7`, `Organization/a-1.Practice-80000`
and `Location/a-80000.Department-21`. The same ids by `_id` search answered
empty Bundles. Adding `ah-practice` changed neither. `_include` added no
entries either: `MedicationRequest:medication` and
`DiagnosticReport:performer:Practitioner` were tried. A target with several
types needs the third part: `DocumentReference:author` answers 400 "3rd
Parameter for _include is required when target has multiple types". athena
still reads each reference, and each 404 is a `not_found` skip. A
production practice answered those reads the same way on 2026-11-17,
through the athenahealth-wide base.

The CapabilityStatement names each type's required search parameters in an
`ah-search-parameter-metadata` extension (`requiredSearchParams`, and
`requiredAnySearchParams` once per alternative). Of the searched types only
MedicationRequest needs more than `patient`: `patient, intent` or `_id`. A
search by `patient` alone answers 403 ("one of the required parameter
combinations [[patient,intent],[_id]] was not provided"), so athena sends
every intent value, `intent=proposal,plan,order,original-order,reflex-order,
filler-order,instance-order,option` (`export::MEDICATION_REQUEST_INTENTS`),
and the skip's `search` names it.

On 2026-11-17 the sandbox answered the RelatedPerson search for its test
patient with 401 and `processing`: "An error occurred while calling the PDP
service (PDP status code: 400 BAD_REQUEST)", while the same token read
everything else. That token came from the Okta server behind the gateway.
With a token from the gateway the same search answered 401 `processing`
"Invalid auth request type". A 401 is otherwise a refused token, so athena renews once
and retries. When the retry is refused too, the export reads `Patient/<id>`
once more: if that read works, the refusal belongs to the one request and is
a `forbidden` skip whose reason quotes the server; if not, the export ends
with exit 3 as for any refused sign-in. Specimen and MedicationDispense
answered empty for that patient.

Ids. FHIR R4 allows 1 to 64 of `A-Z a-z 0-9 - .`. athena applies
`pk_med_fhir::is_id`, the rule `FhirClient`'s reads apply: the same
characters, not only dots, at up to `pk_med_fhir::MAX_ID_LEN` (256), to the
patient id a sign-in names, to `documents download`'s ids and to every id a
server's answer names before athena reads it. Epic issues ids past 64, and
athenahealth publishes no bound. **Unverified**: whether athenahealth issues
any past 64. A reference or attachment naming an id outside the rule is an
`invalid_reference` skip (`pk_med_records::stream::skip::INVALID_REFERENCE`),
in `records export` and in `documents download` alike, and nothing is sent
for it: the server's own data, which asking again does not change. References
are read with `pk_med_fhir::ReferenceTarget::read`: relative, or absolute on
the practice's FHIR base; a `_history` version reads as the resource. A
contained (`#…`), `urn:`, conditional (`Type?…`) or other server's reference
is not followed. A reference whose id the rule refuses comes back apart
(`Err(RefusedReference)`), so it becomes the skip rather than vanishing, when
`is_under` puts it on the practice's base.

Remedies. pk_med_smart's errors carry generic remedies (`pk_med_smart::remedy`,
"sign in again"). athena replaces them with its own commands: a refused code
exchange or ID token names `athena auth login --org <org>`, and a Keychain
item that does not read names `athena auth logout --org <org> --patient <id>`,
then `auth login`.

### Documents and Binary

- `Binary` is read-only (`GET Binary/<id>`), answering FHIR JSON with base64
  `data` and `contentType`. From [Lab & Imaging Documentation
  Retrieval][labimg]: "A JSON array with the Binary will be provided if there
  is an image associated with the document" (`"presentedForm": [{"url":
  "Binary/{logicalID}", "contentType": "image/png"}]`), and "The response will
  display the base64 encoded "data"."
- DocumentReference attachments point at `Binary/<id>` the same way.
- What a patient token sees is what the portal shows: "When called using
  patient 3-legged OAuth ... the response includes only documents and results
  published to the Patient Portal (those without the NOPAT tag)." ([Patient
  Portal Access to Documents/Sensitive Result Filtering][nopat]) A practice
  setting ("Publish clinical notes to patient portal", "Autopublishing of lab
  and imaging results") widens that; the patient cannot.

athena fetches only `Binary` on the signed-in FHIR server's own origin; an
attachment elsewhere is a skip (`unsupported`), never a request with the token.

On 2026-11-17 the sandbox's test patient had 169 DocumentReferences, and
none pointed at a `Binary`. 56 carried the file inline as `data`, `text/html`
or `text/plain`. The other 113 were clinical notes with no file: the
attachment has `contentType` `application/octet-stream` and `_data` with a
`data-absent-reason` of `unknown`. Two DiagnosticReports carried an inline
`presentedForm`. `documents download --all` saved the 58 inline files and
skipped the 113 as `no_file`.

A production patient's DocumentReferences showed both kinds on 2026-11-17,
inline `text/html` files and notes without one, and a third. One
DocumentReference had four `image/png` attachments at `Binary/<id>`, and
every read of those answered 404 `not-found` ("A resource with that id does
not exist"). They answered the same on the athenahealth-wide base and on the
practice's own base, with `Binary` granted. Each is a `not_found` skip.
**Unverified**: why.

`documents download` names a file `<date>_<name>_<id>.<ext>`: at most 60
characters of the name, and the id whole up to 64 characters. A longer id
keeps its first 47 characters and the first 16 hex digits of its SHA-256, so
a name stays near 150 bytes (a file name allows 255) and two ids sharing a
prefix still name two files.

### Imaging

- The R4 CapabilityStatement has **no `ImagingStudy`**.
- `Media` is served by read only, with no search, so athena reaches it through
  a `DiagnosticReport.media` link.
- What a patient token can download is the imaging *document*: a
  `DiagnosticReport` with a `presentedForm` `Binary` (an image of the report,
  such as `image/png`, or a PDF). No DICOM and no DICOMweb are offered to a
  patient. Studies themselves come from the imaging facility.

## The directory of practices

"Click here to download a FHIR Resource Bundle containing the global FHIR R4
base URL as well as all site-specific FHIR R4 base URLs for all of
athenahealth's live customers." ([FHIR Base URLs][base])

- `https://service-base-urls.api.fhir.athena.io/athena-fhir-service-base-urls.json`:
  a `collection` Bundle of about 135 MB (2026-11-12: 127,576 `Organization`
  and 23,434 `Endpoint` entries). Each `Organization` carries its name,
  address and NPI, the extensions
  `https://fhir.athena.io/StructureDefinition/ah-practice`
  (`Organization/a-1.Practice-<practiceid>`) and `.../ah-brand`
  (`Organization/a-<practiceid>.Brand-<brandid>`), and references to two
  `Endpoint`s: the global base and its brand base (which carries the chart
  sharing group).
- The CSV files beside it "are updated daily".

`athena orgs refresh` downloads the Bundle once and keeps only name, city,
state, ZIP and the three ids; `orgs search` reads that copy.

## Retries

From [Timeouts, Retries, and Backoff][retries]: "Do implement Retry for status
codes 502, 503 and 504", "Do use an exponential backoff strategy", "Do
consider use of jitter", and "Don't implement Retry for status code 429
without ensuring the failed request is related to excessive concurrent
requests and not the exceeding of a daily limit."

athena reads through medical-common's `pk_med_fhir::FhirClient` with this
`RetryPolicy` (`fhir::retry_policy`): four requests in all, waiting 1 s, 2 s
and 4 s, or the server's `Retry-After`, each wait lengthened by up to a
quarter (jitter), never more than 30 s. A 502, 503 or 504 is asked again;
another 5xx is not. A 429 is asked again only when its `Retry-After` is 30 s
or less; one without, or asking for longer, fails at once. A connection that
failed, and a 403 whose body is an HTML page (athenahealth's edge), are
asked again the same way. Each wait is a line on stderr unless `--quiet`.
v0.2.x capped a longer `Retry-After` on a 5xx at 30 s and did not retry the
edge's 403.

One request goes at a time. A search hands each page over as it is read
(`search_pages`), so a page that fails keeps the pages before it in the
stream, and the search is an `upstream` skip. When three requests in a row
fail that way (a 429, a 5xx, the edge's block or a failed connection, after
their retries; `fhir::Client` counts them from `FhirClient`'s results) the
export stops with `complete: false` and exit 5 (the circuit breaker). A
failed `Patient` read stops it at once: exit 5, or exit 3 when the read is
refused (403), with the remedy to sign in again, since the practice may have
turned off access for the app.

An answer that is not a 2xx maps to an exit code by medical-common's
`pk_med_fhir::outcome::status_error`. The `OperationOutcome`'s issue code is
used when the contract knows it: `login`, `unknown` or `expired` is exit 3
(sign in again), `forbidden`, `suppressed` or `security` exit 3 (refused),
`not-found` or `deleted` exit 4. Otherwise the HTTP status decides: 401 and
403 exit 3 (in an export, a 401 on one search, read or `Binary` fetch that
outlasts a renewal while the same sign-in still reads `Patient/<id>` is a
`forbidden` skip instead; see FHIR R4), 404 and 410 exit 4, any other 4xx exit 5 (an `unsupported`
skip), and 5xx exit 5. A 429, a 5xx, or a 403 whose body is an HTML page
(athenahealth's edge rather than the FHIR server) is exit 5 with no reason,
so it counts toward the circuit breaker instead of reading as a refusal.

A 2xx search can say it left results out: an `OperationOutcome` entry of
mode `outcome` with an issue coded `suppressed`, which Epic sends as a
warning naming a sub-resource the client may not read. athena reads each
search's outcomes with `pk_med_fhir::SearchPage::withheld` (medical-common,
since v0.4.9). The export keeps the results that were sent and writes one
`forbidden` skip per type and distinct line (medical-common DESIGN §1.4):
`resource_type` set, `id` and `search` unset, the server's line as
`reason`. A reader takes it as resources of that type the server did not
serve. `documents list --json` and `documents download --all --json` add a
`withheld` array of the same `{resource_type, code, reason}` when there is
any, since a `documents/v1` skip names a document and these name none; both
also print a warning to stderr per line. `withheld` is medical-common's
extension of `documents/v1` for provider CLIs (DESIGN §1.7), not a field of
cli-common's `documents/v1` profile (`pk-cli-documents`); a `documents/v1`
reader that does not know it ignores it. When `documents/v1` gains a shared
field for what a source left out, athena writes that field beside
`withheld` for the rest of `/v1`. **Unverified**: whether athenahealth
sends such an outcome; the mock's `withheld` mode serves Epic's shape.

## The sandbox

From the [Onboarding Overview][onboarding]: "We have also configured a dummy
patient record and patient login credentials for PHR testing in Preview:
Practice ID: #80000 Patient ID: #14545 Email / Password (for Login with
athenahealth)": `phrtest_preview@mailinator.com` and the password on that
page. Preview practice 195900 also holds synthetic test patients
(`a-195900.E-60178` to `a-195900.E-60184`, family name `Sandboxtest`;
[Testing in the Sandbox][sandbox]). A sign-in to the sandbox needs a Preview
client id.

On 2026-11-17 athena v0.3.8 signed in as the test patient through the
gateway. The sign-in read as `self`, was granted every type, and carried a
refresh token. `records export` was complete, with one `forbidden` skip
(RelatedPerson) and a `not_found` skip for each referenced read. It was
complete again on a renewed token.

## Practice migrations

A practice that moves to a new athenaOne practice id gets a new FHIR base and
a new patient portal host, and its patients' portal accounts are registered
again on the new portal. For athena that is a new practice profile and a new
sign-in per patient. Records from before the move stay on the old practice,
so the old profile is exported first, and kept while the old portal is.

## Traps

| Trap | What athena does |
|---|---|
| The SMART configuration's `grant_types_supported` lists `authorization_code` and `client_credentials` only, though refresh works | Relies on the token documentation, not that list |
| `issuer` is an Okta URL, not the FHIR host | Compares an RFC 9207 `iss` on the callback with the discovered issuer only |
| `expires_in` is a number in one documented example and a string (`"300"`) in another | Reads both |
| A refresh without `scope` | Always sends the granted scope |
| The redirect URI is matched exactly, port included | Fixed registered ports, never an ephemeral one |
| The button's script builds a production authorize URL without `aud` or PKCE | Own start page with the button's markup |
| A patient token sees only what the portal publishes (`NOPAT`) | Documents it; nothing to work around |
| One sign-in grants one patient record | One token set per (practice profile, patient) |
| `Appointment` search takes no `patient` parameter | Not exported |
| `Media` has no search | Read through `DiagnosticReport.media` |
| A 429 can mean the daily limit, where a retry only adds load | Retried only with a short `Retry-After` |
| A search by `patient` alone for MedicationRequest answers 403 | Sends every `intent` value with `patient` |
| The sandbox and a production practice answer 404 to every read of a referenced Practitioner, Organization, Location or Medication, with all four granted | Each is a `not_found` skip; the export stays complete |
| A clinical note can carry no file (`_data` with `data-absent-reason` `unknown`) | A `no_file` skip in `documents download` |
| An image attachment's `Binary/<id>` can answer 404 on every base, with `Binary` granted | A `not_found` skip; the export stays complete |
| The RelatedPerson search can answer 401 ("error calling the PDP service", or "Invalid auth request type") to a token that reads everything else | After one renewal, a `forbidden` skip if `Patient/<id>` still reads; otherwise exit 3 |
| A new app's client id can be unknown to athenahealth's API gateway after Okta accepts it. Sign-in and consent work, then the token endpoint answers 401 `{"error":"InvalidClient","detailedmessage":"The provided client is not valid in the preview environment"}` (seen 2026-11-16, about an hour after the app was created; it cleared by itself, for the production id 16 to 17 hours and for the Preview id about 18 hours after the apps were created). A token request with a made-up code tells the two apart: 401 `InvalidClient` while the gateway does not know the client, 400 `invalid_grant` once it does. The issuer's own Okta token endpoint accepts the client meanwhile, and the FHIR API reads with the token it issues, but its answer names no patient and a token renewed there is refused (401) | Exit 5 with that message; nothing is stored. athena uses only the token endpoint the SMART configuration names; sign in again once a made-up code gets 400 |

[r233]: https://docs.athenahealth.com/api/resources/233-release-developer-console-allows-self-service-onboarding-all-apps-using-only
[r227]: https://docs.athenahealth.com/api/resources/227-release-updated-enable-authentication-against-multiple-athenaone-practices
[certified]: https://docs.athenahealth.com/api/guides/certified-apis
[naming]: https://docs.athenahealth.com/api/guides/app-naming-guidelines
[terms]: https://docs.athenahealth.com/api/guides/termsofservice
[onboarding]: https://docs.athenahealth.com/api/guides/onboarding-overview
[base]: https://docs.athenahealth.com/api/guides/base-fhir-urls
[token]: https://docs.athenahealth.com/api/guides/token-endpoint
[authorize]: https://docs.athenahealth.com/api/guides/authorize-endpoint
[authz]: https://docs.athenahealth.com/api/guides/authorization-overview
[oauth-extra]: https://docs.athenahealth.com/api/guides/additional-oauth-endpoints
[labimg]: https://docs.athenahealth.com/api/workflows/fhir-r4-lab-and-imaging-documentation-retrieval
[nopat]: https://docs.athenahealth.com/api/workflows/fhir-r4-api-patient-portal-access-to-documents-sensitive-result-filtering
[retries]: https://docs.athenahealth.com/api/guides/timeouts-retries-and-backoff
[sandbox]: https://docs.athenahealth.com/api/guides/testing-sandbox
