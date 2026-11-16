# Getting a client id

athena signs in to athenahealth as a registered app. athenahealth issues the
app a **client id**. athena keeps it in your local configuration, never in
this repository and never in code, so each household registers its own app.
There is no secret: athena is a public client and proves each sign-in with
PKCE.

Registration is free for apps that use only athenahealth's Certified APIs.
It takes about 15 minutes and needs no contract
([docs/api.md](api.md#route-certified-apis-as-a-phr-app)).

## 1. A Developer Portal account

Open <https://developer.api.athena.io/ams-portal/>. Sign in, or choose **Sign
up** and give your name and email. athenahealth emails an invitation. Follow
it to set a password and a phone for sign-in codes.

## 2. A Preview app (the sandbox)

1. Choose **(+) Create a New App**.
2. Choose **My app will use Certified APIs ONLY**, then the category **3-Legged
   OAuth for Patients**, tick **I confirm that I am building a PHR app**, then
   choose **Continue to App Details**.
3. Fill in the form:

   | Field | Value |
   |---|---|
   | Environment | Preview |
   | App name | Your own brand plus what it does, such as "<Family name> Health Records". athenahealth's [naming guidelines][naming] refuse "athena", "athenahealth", "app", "application", "PHR", "CAPI", "test", environment words and people's names. |
   | Website | Your copy of athena, or <https://github.com/piekstra/athena-cli> |
   | Description | Command-line tool with which a patient or caregiver downloads their own records from athenahealth practices to their own computer. |
   | Intended end users | The patient and approved family members or caregivers (a caregiver signs in for a child) |
   | OAuth client type | Public (native) |
   | Token authentication | PKCE, no client secret |
   | Redirect URLs | `http://127.0.0.1:47816/callback` and `http://127.0.0.1:47817/callback` |

4. Read the linked terms ([summary](api.md#the-terms-a-registration-agrees-to)),
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

Repeat step 2 with Environment **Production**. If the name is refused as a
duplicate, add a word ("... Records Export").

## 4. Give athena the client ids

```sh
athena config set client-id <PREVIEW_CLIENT_ID> --env preview
athena config set client-id <PRODUCTION_CLIENT_ID> --env production
athena orgs list       # CLIENT_ID_CONFIGURED is true for the sandbox and your practices
```

A practice whose FHIR server is not athenahealth's own (`--env custom`) takes
its client id on its profile instead: `athena config set practice <slug> ...
--client-id <ID>`.

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
athena auth login --org sandbox --label sandbox
athena auth status --json
athena records export --patient sandbox --ndjson | head -c 300
```

[naming]: https://docs.athenahealth.com/api/guides/app-naming-guidelines
[onboarding]: https://docs.athenahealth.com/api/guides/onboarding-overview
