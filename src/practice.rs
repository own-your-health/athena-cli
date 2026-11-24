//! Which practice a record is at, for the export header's `organization`.
//!
//! An athenahealth-wide sign-in's practice profile names no practice: the
//! patient picks one record, at one practice, in athenahealth's prompt. No
//! resource the server serves carries that practice's name. Every id it
//! serves starts with the practice id (`a-<practiceid>.E-<chart>`), but
//! Encounter's `serviceProvider` and `location` and the `ah-practice`
//! extension are bare references, and each read of an Organization or
//! Location answers 404 (`docs/api.md`). So the name comes from the
//! household's own practice profile for that practice id, or from
//! athenahealth's published directory when it lists one name for it.

use std::collections::BTreeSet;

use crate::config::{Config, Practice, SANDBOX_ORG};
use crate::directory::Directory;
use crate::env::Environment;

/// The athenaOne practice id at the front of an id athenahealth serves:
/// `a-<practiceid>.E-<chart>` gives `<practiceid>`.
pub fn practice_id_of(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("a-")?;
    let (practice, tail) = rest.split_once('.')?;
    (!practice.is_empty() && practice.bytes().all(|b| b.is_ascii_digit()) && !tail.is_empty())
        .then_some(practice)
}

/// The header's name for a sign-in's organization, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    /// City and state, when every directory entry for the practice shares
    /// them.
    pub location: Option<String>,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The sign-in's own practice profile: it names a practice, or the
    /// sign-in is not athenahealth-wide production.
    SignIn,
    /// Another practice profile with the record's practice id.
    Profile(String),
    /// athenahealth's directory, which lists one name for the practice id.
    Directory,
    /// The sign-in's own profile, since nothing else names the record's
    /// practice: the practice id is given so a profile can be added for it.
    Unnamed(String),
}

/// The name the export header gives the organization of a sign-in at
/// practice profile `org` for `patient`. `directory` is read only for an
/// athenahealth-wide production sign-in.
pub fn header_name(
    config: &Config,
    org: &str,
    patient: &str,
    directory: impl FnOnce() -> Option<Directory>,
) -> Named {
    let own = config
        .practices
        .get(org)
        .cloned()
        .or_else(|| (org == SANDBOX_ORG).then(Practice::sandbox));
    let sign_in = |source| Named {
        name: own
            .as_ref()
            .map(|p| p.display_name(org))
            .unwrap_or_else(|| org.to_string()),
        location: None,
        source,
    };
    let athenahealth_wide = own.as_ref().is_some_and(|p| {
        p.environment == Environment::Production && p.practice_id.is_none() && p.fhir_base.is_none()
    });
    let practice = match practice_id_of(patient) {
        Some(p) if athenahealth_wide => p,
        _ => return sign_in(Source::SignIn),
    };

    let directory = directory();
    let listed: Vec<_> = directory
        .iter()
        .flat_map(|d| d.entries.iter())
        .filter(|e| e.practice_id.as_deref() == Some(practice))
        .collect();
    let places: BTreeSet<_> = listed
        .iter()
        .map(|e| (e.city.clone(), e.state.clone()))
        .collect();
    let location = match places.into_iter().collect::<Vec<_>>().as_slice() {
        [(Some(city), Some(state))] => Some(format!("{city}, {state}")),
        _ => None,
    };

    let profiles: BTreeSet<(String, String)> = config
        .practices
        .iter()
        .filter(|(_, p)| {
            p.environment == Environment::Production && p.practice_id.as_deref() == Some(practice)
        })
        .map(|(slug, p)| (p.display_name(slug), slug.clone()))
        .collect();
    let names: BTreeSet<&String> = profiles.iter().map(|(name, _)| name).collect();
    if names.len() == 1 {
        let (name, slug) = profiles.into_iter().next().unwrap_or_default();
        return Named {
            name,
            location,
            source: Source::Profile(slug),
        };
    }

    let names: BTreeSet<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    if let [name] = names.into_iter().collect::<Vec<_>>().as_slice() {
        return Named {
            name: (*name).to_string(),
            location,
            source: Source::Directory,
        };
    }
    sign_in(Source::Unnamed(practice.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::Entry;

    fn wide() -> Practice {
        Practice {
            name: Some("athenahealth (every practice)".into()),
            environment: Environment::Production,
            ..Practice::default()
        }
    }

    fn at(practice: &str, name: &str) -> Practice {
        Practice {
            name: Some(name.into()),
            environment: Environment::Production,
            practice_id: Some(practice.into()),
            brand_id: Some("1".into()),
            csg_id: Some("2".into()),
            ..Practice::default()
        }
    }

    fn config(practices: &[(&str, Practice)]) -> Config {
        Config {
            practices: practices
                .iter()
                .map(|(slug, p)| (slug.to_string(), p.clone()))
                .collect(),
            ..Config::default()
        }
    }

    fn entry(practice: &str, name: &str, city: &str) -> Entry {
        Entry {
            name: name.into(),
            city: Some(city.into()),
            state: Some("ZZ".into()),
            zip: None,
            practice_id: Some(practice.into()),
            brand_id: Some("1".into()),
            csg_id: None,
            fhir_base: None,
        }
    }

    fn directory(entries: Vec<Entry>) -> Option<Directory> {
        Some(Directory {
            entries,
            ..Directory::default()
        })
    }

    #[test]
    fn the_practice_id_is_the_front_of_the_id() {
        assert_eq!(practice_id_of("a-900001.E-1001"), Some("900001"));
        assert_eq!(practice_id_of("a-1.Practice-900001"), Some("1"));
        for id in [
            "900001.E-1001",
            "a-.E-1",
            "a-9x.E-1",
            "a-900001",
            "a-900001.",
            "e-1",
        ] {
            assert_eq!(practice_id_of(id), None, "{id}");
        }
    }

    #[test]
    fn a_wide_sign_in_takes_the_name_of_the_profile_for_its_practice() {
        let cfg = config(&[("wide", wide()), ("clinic", at("900001", "Example Clinic"))]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
            directory(vec![entry("900001", "Listed Name", "Testville")])
        });
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.location.as_deref(), Some("Testville, ZZ"));
        assert_eq!(named.source, Source::Profile("clinic".into()));
    }

    #[test]
    fn the_directory_names_a_practice_it_lists_once() {
        let cfg = config(&[("wide", wide())]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
            directory(vec![
                entry("900001", "Example Clinic", "Testville"),
                entry("900001", "Example Clinic", "Othertown"),
                entry("900002", "Another Clinic", "Testville"),
            ])
        });
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.location, None, "two cities");
        assert_eq!(named.source, Source::Directory);
    }

    #[test]
    fn a_practice_with_several_names_keeps_the_sign_ins_name() {
        let cfg = config(&[
            ("wide", wide()),
            ("one", at("900001", "Example Clinic")),
            ("two", at("900001", "Example Clinic North")),
        ]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
            directory(vec![
                entry("900001", "Example Clinic", "Testville"),
                entry("900001", "Example Pediatrics", "Testville"),
            ])
        });
        assert_eq!(named.name, "athenahealth (every practice)");
        assert_eq!(named.source, Source::Unnamed("900001".into()));
    }

    #[test]
    fn two_profiles_with_one_name_name_it() {
        let cfg = config(&[
            ("wide", wide()),
            ("one", at("900001", "Example Clinic")),
            ("two", at("900001", "Example Clinic")),
        ]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || None);
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.source, Source::Profile("one".into()));
    }

    #[test]
    fn without_a_directory_or_profile_the_practice_is_unnamed() {
        let cfg = config(&[("wide", wide())]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || None);
        assert_eq!(named.name, "athenahealth (every practice)");
        assert_eq!(named.location, None);
        assert_eq!(named.source, Source::Unnamed("900001".into()));
    }

    #[test]
    fn a_practice_sign_in_and_the_sandbox_keep_their_own_name_unread() {
        let cfg = config(&[
            ("clinic", at("900001", "Example Clinic")),
            ("other", at("900002", "Other")),
        ]);
        let unread = || -> Option<Directory> { panic!("the directory is not read") };
        let named = header_name(&cfg, "clinic", "a-900002.E-1001", unread);
        assert_eq!(
            (named.name.as_str(), named.source),
            ("Example Clinic", Source::SignIn)
        );
        let named = header_name(&cfg, SANDBOX_ORG, "a-900002.E-1001", unread);
        assert_eq!(named.name, Practice::sandbox().display_name(SANDBOX_ORG));
        assert_eq!(named.source, Source::SignIn);
        let named = header_name(&cfg, "gone", "a-900002.E-1001", unread);
        assert_eq!(
            (named.name.as_str(), named.source),
            ("gone", Source::SignIn)
        );
    }

    #[test]
    fn a_patient_id_in_another_shape_keeps_the_sign_ins_name() {
        let cfg = config(&[("wide", wide()), ("clinic", at("900001", "Example Clinic"))]);
        let named = header_name(&cfg, "wide", "patient-1", || {
            panic!("the directory is not read")
        });
        assert_eq!(named.source, Source::SignIn);
        assert_eq!(named.name, "athenahealth (every practice)");
    }
}
