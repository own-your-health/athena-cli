//! SMART App Launch for a standalone patient app, as athenahealth serves it:
//! the vendor's quirks, the scopes athena asks for, and what a granted scope
//! allows. Discovery, the launch and renewal are `pk_med_smart`'s.

use std::collections::BTreeSet;

use pk_med_smart::{ScopeSet, ScopeVersion, VendorQuirks};

/// athenahealth needs nothing beyond what `pk_med_smart` does by default:
/// `aud` on the authorization (it picks the practice, `docs/api.md`), a
/// `nonce` whenever `openid` is asked for ("Required if requesting the openid
/// scope"), and the granted scope on every refresh ("Required if grant_type
/// is ... refresh_token"). Its `.well-known/smart-configuration` answers
/// first, so `/metadata` is never asked.
///
/// Its `issuer` is its Okta authorization server
/// (`https://athena.okta.com/oauth2/...`), not the FHIR host, and its
/// `grant_types_supported` leaves out `refresh_token` although refresh works;
/// neither is relied on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Athenahealth;

impl VendorQuirks for Athenahealth {}

/// The resource types athena asks to read: athenahealth's list of
/// auto-approved patient scopes for a standalone launch, less the DSTU2-only
/// and proprietary names. athenahealth refuses wildcard scopes, so each is
/// named. Its scope editor offers all of them under FHIR R4 SMART V2; SMART
/// V1 lacks Coverage, Media, MedicationDispense, RelatedPerson and Specimen.
pub const PATIENT_TYPES: &[&str] = &[
    "AllergyIntolerance",
    "Binary",
    "CarePlan",
    "CareTeam",
    "Condition",
    "Coverage",
    "Device",
    "DiagnosticReport",
    "DocumentReference",
    "Encounter",
    "Goal",
    "Immunization",
    "Location",
    "Media",
    "Medication",
    "MedicationDispense",
    "MedicationRequest",
    "Observation",
    "Organization",
    "Patient",
    "Practitioner",
    "Procedure",
    "Provenance",
    "RelatedPerson",
    "ServiceRequest",
    "Specimen",
];

/// The scopes of a standalone patient sign-in: OpenID Connect, the patient
/// picker, a refresh token, and read and search per type in SMART v2
/// spelling (`patient/<Type>.rs`), the product in athenahealth's scope
/// editor that offers every type athena reads (`docs/registration.md`).
/// `version` is `v1` (`patient/<Type>.read`) only for an app that holds
/// SMART V1 scopes alone (`athena config set scope-version v1`).
pub fn scopes(version: ScopeVersion) -> ScopeSet {
    ScopeSet::standalone_patient().read_types(version, PATIENT_TYPES)
}

/// The types a token answer's `scope` grants reading. An answer without
/// `scope`, or with an empty one, grants what was asked for (RFC 6749 §5.1):
/// every one of [`PATIENT_TYPES`].
pub fn granted(scope: Option<&str>) -> BTreeSet<String> {
    match scope.filter(|s| !s.trim().is_empty()) {
        None => PATIENT_TYPES.iter().map(|t| t.to_string()).collect(),
        Some(scope) => granted_types(scope),
    }
}

/// The types athena asks for that `scope` does not grant, in
/// [`PATIENT_TYPES`] order.
pub fn not_granted(scope: Option<&str>) -> Vec<&'static str> {
    let granted = granted(scope);
    PATIENT_TYPES
        .iter()
        .copied()
        .filter(|t| !granted.contains(*t))
        .collect()
}

/// The resource types a granted scope string allows reading, in SMART v1
/// (`patient/Observation.read`, `patient/*.read`) and v2
/// (`patient/Observation.rs`, with or without a `?category=` narrowing)
/// spellings.
pub fn granted_types(scope: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for item in scope.split_whitespace() {
        let Some(rest) = item
            .strip_prefix("patient/")
            .or_else(|| item.strip_prefix("user/"))
        else {
            continue;
        };
        let Some((resource, perms)) = rest.split_once('.') else {
            continue;
        };
        let perms = perms.split('?').next().unwrap_or("");
        let reads = perms == "read"
            || perms == "*"
            || (!perms.is_empty()
                && perms.chars().all(|c| "cruds".contains(c))
                && (perms.contains('r') || perms.contains('s')));
        if !reads {
            continue;
        }
        if resource == "*" {
            out.extend(PATIENT_TYPES.iter().map(|t| t.to_string()));
        } else {
            out.insert(resource.to_string());
        }
    }
    out
}

/// `self` when the signed-in person is the patient the token is for,
/// `proxy` when someone else (a caregiver) signed in, from the id token's
/// `fhirUser`.
pub fn relationship(fhir_user: Option<&str>, patient: &str) -> Option<&'static str> {
    let user = fhir_user?;
    if user == format!("Patient/{patient}") || user.ends_with(&format!("/Patient/{patient}")) {
        Some("self")
    } else {
        Some("proxy")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scope_names_every_type_and_no_wildcard() {
        let scope = scopes(ScopeVersion::V2).to_string();
        assert!(scope.starts_with("openid fhirUser launch/patient offline_access "));
        assert!(scope.contains("patient/DocumentReference.rs"));
        assert!(scope.contains("patient/Binary.rs"));
        assert!(!scope.contains(".read"), "SMART v2 only: {scope}");
        assert_eq!(granted_types(&scope).len(), PATIENT_TYPES.len());
        assert!(!scope.contains('*'));
        assert_eq!(scope.split(' ').count(), 4 + PATIENT_TYPES.len());
    }

    #[test]
    fn v1_is_asked_only_when_configured() {
        let scope = scopes(ScopeVersion::V1).to_string();
        assert!(scope.contains("patient/Patient.read"));
        assert!(!scope.contains(".rs"));
        assert_eq!(granted_types(&scope).len(), PATIENT_TYPES.len());
    }

    #[test]
    fn what_a_token_answer_leaves_out_is_named() {
        assert!(
            not_granted(None).is_empty(),
            "no scope grants what was asked"
        );
        assert!(not_granted(Some(" ")).is_empty());
        assert_eq!(
            not_granted(Some("openid fhirUser launch/patient offline_access")).len(),
            PATIENT_TYPES.len()
        );
        let all_but_two = PATIENT_TYPES
            .iter()
            .filter(|t| !matches!(**t, "Coverage" | "Specimen"))
            .map(|t| format!("patient/{t}.read"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(not_granted(Some(&all_but_two)), ["Coverage", "Specimen"]);
    }

    #[test]
    fn granted_types_reads_v1_and_v2_spellings() {
        let granted = granted_types(
            "openid patient/Patient.read patient/Observation.rs?category=http://x|laboratory \
             patient/Condition.s patient/Goal.c launch/patient",
        );
        assert!(granted.contains("Patient"));
        assert!(granted.contains("Observation"));
        assert!(granted.contains("Condition"));
        assert!(!granted.contains("Goal"));
        assert!(!granted.contains("launch"));
        assert_eq!(granted_types("patient/*.read").len(), PATIENT_TYPES.len());
    }

    #[test]
    fn relationship_comes_from_fhir_user() {
        assert_eq!(
            relationship(Some("https://x/fhir/r4/Patient/a-1.E-2"), "a-1.E-2"),
            Some("self")
        );
        assert_eq!(
            relationship(Some("RelatedPerson/r-1"), "a-1.E-2"),
            Some("proxy")
        );
        assert_eq!(
            relationship(Some("Patient/a-1.E-9"), "a-1.E-2"),
            Some("proxy")
        );
        assert_eq!(relationship(None, "a-1.E-2"), None);
    }
}
