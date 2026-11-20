# Registering an app for athena

athena signs in to athenahealth as a registered app, and athenahealth issues
the app a **client id**. athena ships none: each user registers their own
app, under their own Developer Portal account, and keeps its client id in
their local configuration (never in this repository, never in code). There
is no secret: athena is a public client and proves each sign-in with PKCE.

Registration is free for apps that use only athenahealth's Certified APIs.
It takes about 15 minutes, needs no contract, and has no review
([docs/api.md](api.md#route-certified-apis-as-a-phr-app)).

`athena registration` prints every value below, with your configuration's
redirect ports (`--env preview` for the sandbox app, `--json` for
`registration/v1`).

| Setting | Value |
|---|---|
| Launch type | SMART standalone patient launch: athena starts the sign-in itself; there is no launch URL |
| Client type | Public (native), no client secret |
| PKCE | Required, S256 |
| Redirect URIs | `http://127.0.0.1:47816/callback` and `http://127.0.0.1:47817/callback` |
| Scopes | FHIR R4 SMART V2, read and search for 26 types (section 2, step 5) |

## 1. A Developer Portal account

Open <https://developer.api.athena.io/ams-portal/>. Sign in, or choose **Sign
up** and give your name and email. athenahealth emails an invitation. Follow
it to set a password and a phone for sign-in codes.

## 2. A Preview app (the sandbox)

The Preview app reaches only athenahealth's sandbox and its synthetic
patient. To read your own records you need the Production app (section 3);
register Preview first if you want to try athena on the sandbox.

1. Choose **(+) Create a New App**.
2. Choose **My app will use Certified APIs ONLY**, then the category **3-Legged
   OAuth for Patients**, tick **I confirm that I am building a PHR app**, then
   choose **Continue to App Details**.
3. Fill in the form (`athena registration --env preview` prints it, with your
   redirect ports):

   | Field | Value |
   |---|---|
   | Environment | Preview |
   | App name | Your own brand plus what it does, such as "<Brand> Health Records". athenahealth's [naming guidelines][naming] refuse "athena", "athenahealth", "app", "application", "PHR", "CAPI", "test", environment words and people's names ([What the portal refused](#what-the-portal-refused-and-what-works)). |
   | Website | Your copy of athena, or <https://github.com/piekstra/athena-cli> |
   | Description | Command-line tool with which a patient or caregiver downloads their own records from athenahealth practices to their own computer. |
   | Intended end users | The patient and approved family members or caregivers (a caregiver signs in for a child) |
   | OAuth client type | Public (native) |
   | Token authentication | PKCE, no client secret |
   | Redirect URLs | `http://127.0.0.1:47816/callback` and `http://127.0.0.1:47817/callback` |

4. Read the linked terms ([summary](#the-terms-you-agree-to)),
   tick the box and choose **Create Application**. Copy the client id.
5. On the app's **Scopes** tab, open the product **FHIR R4 SMART V2** and
   tick read (`.r`) and search (`.s`) for each of these types, then choose
   **Confirm Scopes**:

   AllergyIntolerance, Binary, CarePlan, CareTeam, Condition, Coverage,
   Device, DiagnosticReport, DocumentReference, Encounter, Goal, Immunization,
   Location, Media, Medication, MedicationDispense, MedicationRequest,
   Observation, Organization, Patient, Practitioner, Procedure, Provenance,
   RelatedPerson, ServiceRequest, Specimen.

   athenahealth adds `patient/<Type>.rs` and category-limited Condition and
   Observation scopes on its own. athena asks for `patient/<Type>.rs` for
   exactly these types (`src/smart.rs`, `PATIENT_TYPES`). A type you leave
   out is reported as a `forbidden` skip in each export.

   The editor also lists athenaOne, Event Notifications, FHIR DSTU2 and FHIR
   R4 SMART V1; athena needs none of them. SMART V1 offers only 21 of the
   types (no Coverage, Media, MedicationDispense, RelatedPerson or Specimen).
   `openid`, `fhirUser`, `offline_access` and `launch/patient` are not in the
   editor: athenahealth approves them for every 3-legged app
   ([docs/api.md](api.md#scopes)).

### An app registered before athena 0.3.3

The steps before athena 0.3.3 asked for `patient/<Type>.read` scopes (FHIR R4
SMART V1), and athena asked in that spelling. From 0.3.3 it asks for
`patient/<Type>.rs` (SMART V2). Add the SMART V2 scopes of step 5 to the app
before signing in again. Until then, `athena config set scope-version v1`
keeps the old request. A sign-in that is granted no read scope is refused
and not stored; one granted only some prints the types left out, and each
export reports those as `forbidden` skips. A sign-in already stored keeps
the scope it was granted when it renews.

## 3. A Production app

Repeat section 2 with Environment **Production** (`athena registration`
prints the form). If the name is refused as a duplicate, add a word
("... Records Export").

## 4. Give athena the client ids

The client id is on the app's page in the portal. It is not a password, but
keep it to yourself: athenahealth's terms keep the "Keys" it issues to the
registrant ([The terms you agree to](#the-terms-you-agree-to)).

```sh
athena config set client-id <PREVIEW_CLIENT_ID> --env preview
athena config set client-id <PRODUCTION_CLIENT_ID> --env production
athena info | jq .client_ids     # each environment: configured, and the id's last four characters
athena orgs list                 # CLIENT_ID_CONFIGURED is true for the sandbox and your practices
```

`athena --profile <NAME> auth status` shows the same, and each sign-in's
client id by its last four characters. `athena config show` prints the ids
whole. A command that needs a client id that is not configured exits 2 and
names `athena registration --env <ENV>`, this page and the `config set`
command.

A practice whose FHIR server is not athenahealth's own (`--env custom`) takes
its client id on its profile instead: `athena config set practice <slug> ...
--client-id <ID>`.

## What the portal refused, and what works

| Refused | Workaround |
|---|---|
| An app name with "athena", "athenahealth", "app", "application", "PHR", "CAPI", "test", "temp", environment words ("prod", "preview", "dev"), usernames, email addresses, URLs or people's names ([naming guidelines][naming]) | Your own brand plus what it does: "<Brand> Health Records" |
| A name another app already has, such as the Production app named like the Preview one | Add a word: "<Brand> Health Records Export" |
| Wildcard scopes (`patient/*.read`) | Each type ticked on its own (section 2, step 5) |
| Coverage, Media, MedicationDispense, RelatedPerson and Specimen under FHIR R4 SMART V1 (the product does not offer them) | FHIR R4 SMART V2 |

The portal accepted `http://127.0.0.1:<port>/callback` redirect URLs in both
environments (2026-11), so athena needs no https forwarding page.

## How long it takes

| Step | Time |
|---|---|
| Sign-up to a working portal account | Minutes: the invitation email, a password, a phone code |
| Review | None for an app that uses only Certified APIs: Preview and Production credentials are issued when the app is created, and a Production app can read at every athenaOne practice once a patient consents |
| The client id reaching athenahealth's API gateway | 16 to 18 hours after the app was created (2026-11). Until then sign-in and consent work, then `auth login` exits 5 and stores nothing, with the message "answered HTTP 401 (InvalidClient)". Sign in again later |

A practice can switch an app off for its own patients on its Manage PHR
Access page; athena then reads nothing at that practice.

## The terms you agree to

The portal's [Developer Portal Terms and Conditions][terms], in short
(athenahealth's wording, with the rest of the terms, is in
[docs/api.md](api.md#the-terms-a-registration-agrees-to)):

- **Keys stay yours.** "You may not sell, transfer, sublicense, or otherwise
  disclose your account or Keys to any other party", and "You are
  responsible for all activities that occur using your account and Keys".
- **No scraping, no resale.** No "web scraping," "data scraping," or "screen
  scraping"; no selling or redistributing "access to the API" without
  athenahealth's written approval. athena reads the API only.
- **Accurate registration.** "providing current, complete, and accurate
  information", updated when it changes.
- **Laws.** Use complies with federal, state and local law, HIPAA named.
- **Termination.** athenahealth "may change, suspend, or discontinue the API
  and suspend or terminate your use of the API and Materials ... at any time
  for any reason, with or without notice".
- **Indemnity and liability.** You indemnify athenahealth for use in breach
  of the terms or the law; athenahealth's liability to you is capped at
  $100. Massachusetts law governs.

## Redirect ports

athenahealth matches the redirect URL exactly, port included, so athena
listens on fixed ports: 47816, or 47817 when 47816 is taken. To use others,
change both places:

```sh
athena config set redirect-ports 50123,50124
```

and replace the app's redirect URLs with `http://127.0.0.1:50123/callback`
and `http://127.0.0.1:50124/callback`.

## Trying it on the sandbox

athenahealth publishes a synthetic patient and its portal login for PHR
testing in Preview: practice 80000, patient 14545, on the [Onboarding
Overview][onboarding] page. With the Preview client id configured:

```sh
athena profiles add sandbox
export ATHENA_PROFILE=sandbox
athena auth login --org sandbox --label sandbox
athena auth status --json
athena records export --patient sandbox --ndjson | head -c 300
```

A new client id can take up to 18 hours to work ([How long it takes](#how-long-it-takes)).

[naming]: https://docs.athenahealth.com/api/guides/app-naming-guidelines
[onboarding]: https://docs.athenahealth.com/api/guides/onboarding-overview
[terms]: https://docs.athenahealth.com/api/guides/termsofservice
