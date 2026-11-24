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

use pk_cli_core::CliError;

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
    /// one of each.
    pub location: Option<String>,
    pub source: Source,
    /// Why the directory cache did not read, when it was needed and did not.
    /// The name then comes from the practice profiles alone.
    pub directory_error: Option<String>,
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

/// A name as compared: trimmed, inner whitespace collapsed, lowercase. Two
/// entries that differ only in case or spacing are one name.
fn key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A name as written into the header: trimmed, inner whitespace collapsed.
fn tidy(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The one name of `names`, by [`key`], in the spelling it first came in.
fn single<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut seen = BTreeSet::new();
    let distinct: Vec<&str> = names
        .into_iter()
        .filter(|name| {
            let k = key(name);
            !k.is_empty() && seen.insert(k)
        })
        .collect();
    match distinct.as_slice() {
        [name] => Some(tidy(name)),
        _ => None,
    }
}

/// The name the export header gives the organization of a sign-in at
/// practice profile `org` for `patient`. `directory` is read only for an
/// athenahealth-wide production sign-in; an error from it is reported in
/// [`Named::directory_error`] and never fails the export.
pub fn header_name(
    config: &Config,
    org: &str,
    patient: &str,
    directory: impl FnOnce() -> Result<Option<Directory>, CliError>,
) -> Named {
    let own = config
        .practices
        .get(org)
        .cloned()
        .or_else(|| (org == SANDBOX_ORG).then(Practice::sandbox));
    let own_name = own
        .as_ref()
        .map(|p| p.display_name(org))
        .unwrap_or_else(|| org.to_string());
    let athenahealth_wide = own.as_ref().is_some_and(|p| {
        p.environment == Environment::Production && p.practice_id.is_none() && p.fhir_base.is_none()
    });
    let practice = match practice_id_of(patient) {
        Some(p) if athenahealth_wide => p,
        _ => {
            return Named {
                name: own_name,
                location: None,
                source: Source::SignIn,
                directory_error: None,
            }
        }
    };

    let (directory, directory_error) = match directory() {
        Ok(d) => (d, None),
        Err(e) => (None, Some(e.to_string())),
    };
    let listed: Vec<_> = directory
        .iter()
        .flat_map(|d| d.entries.iter())
        .filter(|e| e.practice_id.as_deref() == Some(practice))
        .collect();
    let places: BTreeSet<_> = listed
        .iter()
        .map(|e| (e.city.as_deref().map(key), e.state.as_deref().map(key)))
        .collect();
    // One place across every entry: the first entry's spelling of it.
    let location = match places.len() {
        1 => listed.first().and_then(|e| {
            Some(format!(
                "{}, {}",
                tidy(e.city.as_deref()?),
                tidy(e.state.as_deref()?)
            ))
        }),
        _ => None,
    };
    let named = |name: String, source| Named {
        name,
        location: location.clone(),
        source,
        directory_error: directory_error.clone(),
    };

    // BTreeMap order: the profiles by slug, so the slug reported is stable.
    let profiles: Vec<(String, &String)> = config
        .practices
        .iter()
        .filter(|(_, p)| {
            p.environment == Environment::Production && p.practice_id.as_deref() == Some(practice)
        })
        .map(|(slug, p)| (p.display_name(slug), slug))
        .collect();
    if let Some(name) = single(profiles.iter().map(|(name, _)| name.as_str())) {
        if let Some((_, slug)) = profiles.first() {
            return named(name, Source::Profile((*slug).clone()));
        }
    }
    if let Some(name) = single(listed.iter().map(|e| e.name.as_str())) {
        return named(name, Source::Directory);
    }
    Named {
        name: own_name,
        location: None,
        source: Source::Unnamed(practice.to_string()),
        directory_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::Entry;

    const WIDE_NAME: &str = "athenahealth (every practice)";

    fn wide() -> Practice {
        Practice {
            name: Some(WIDE_NAME.into()),
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

    fn entry(practice: &str, name: &str, city: Option<&str>) -> Entry {
        Entry {
            name: name.into(),
            city: city.map(str::to_string),
            state: Some("ZZ".into()),
            zip: None,
            practice_id: Some(practice.into()),
            brand_id: Some("1".into()),
            csg_id: None,
            fhir_base: None,
        }
    }

    fn directory(entries: Vec<Entry>) -> Result<Option<Directory>, CliError> {
        Ok(Some(Directory {
            entries,
            ..Directory::default()
        }))
    }

    fn none() -> Result<Option<Directory>, CliError> {
        Ok(None)
    }

    fn unread() -> Result<Option<Directory>, CliError> {
        panic!("the directory is not read")
    }

    #[test]
    fn the_practice_id_is_the_front_of_the_id() {
        assert_eq!(practice_id_of("a-900001.E-1001"), Some("900001"));
        assert_eq!(practice_id_of("a-1.Practice-900001"), Some("1"));
        for id in [
            "900001.E-1001",
            "a-.E-1",
            "a-900001x.E-1",
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
            directory(vec![entry("900001", "Listed Name", Some("Testville"))])
        });
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.location.as_deref(), Some("Testville, ZZ"));
        assert_eq!(named.source, Source::Profile("clinic".into()));
        assert_eq!(named.directory_error, None);
    }

    #[test]
    fn the_directory_names_a_practice_it_lists_once() {
        let cfg = config(&[("wide", wide())]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
            directory(vec![
                entry("900001", "Example Clinic", Some("Testville")),
                entry("900001", "Example Clinic", Some("Testville")),
                entry("900002", "Another Clinic", Some("Othertown")),
            ])
        });
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.location.as_deref(), Some("Testville, ZZ"));
        assert_eq!(named.source, Source::Directory);
    }

    #[test]
    fn a_location_needs_one_city_and_state_on_every_entry() {
        let cfg = config(&[("wide", wide())]);
        for entries in [
            vec![
                entry("900001", "Example Clinic", Some("Testville")),
                entry("900001", "Example Clinic", Some("Othertown")),
            ],
            vec![entry("900001", "Example Clinic", None)],
            vec![
                entry("900001", "Example Clinic", Some("Testville")),
                entry("900001", "Example Clinic", None),
            ],
        ] {
            let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
                directory(entries.clone())
            });
            assert_eq!(named.source, Source::Directory);
            assert_eq!(named.location, None, "{entries:?}");
        }
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
            directory(vec![
                entry("900001", "Example Clinic", Some("Testville")),
                entry("900001", "Example Clinic", Some(" testville ")),
            ])
        });
        assert_eq!(
            named.location.as_deref(),
            Some("Testville, ZZ"),
            "spelling only"
        );
    }

    #[test]
    fn names_that_differ_only_in_case_or_spacing_are_one() {
        let cfg = config(&[("wide", wide())]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
            directory(vec![
                entry("900001", " Example  Clinic", Some("Testville")),
                entry("900001", "EXAMPLE CLINIC", Some("Testville")),
            ])
        });
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.source, Source::Directory);
    }

    #[test]
    fn a_directory_that_lists_other_practices_leaves_it_unnamed() {
        let cfg = config(&[("wide", wide())]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", || {
            directory(vec![entry("900002", "Another Clinic", Some("Testville"))])
        });
        assert_eq!(named.name, WIDE_NAME);
        assert_eq!(named.location, None);
        assert_eq!(named.source, Source::Unnamed("900001".into()));
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
                entry("900001", "Example Clinic", Some("Testville")),
                entry("900001", "Example Pediatrics", Some("Testville")),
            ])
        });
        assert_eq!(named.name, WIDE_NAME);
        assert_eq!(named.location, None);
        assert_eq!(named.source, Source::Unnamed("900001".into()));
    }

    #[test]
    fn two_profiles_with_one_name_name_it_by_the_first_slug() {
        let cfg = config(&[
            ("wide", wide()),
            ("two", at("900001", "example clinic")),
            ("one", at("900001", "Example Clinic")),
        ]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", none);
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.source, Source::Profile("one".into()));
    }

    #[test]
    fn a_profile_in_another_environment_does_not_name_it() {
        let mut preview = at("900001", "Example Clinic");
        preview.environment = Environment::Preview;
        let cfg = config(&[("wide", wide()), ("test", preview)]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", none);
        assert_eq!(named.source, Source::Unnamed("900001".into()));
    }

    #[test]
    fn without_a_directory_or_profile_the_practice_is_unnamed() {
        let cfg = config(&[("wide", wide())]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", none);
        assert_eq!(named.name, WIDE_NAME);
        assert_eq!(named.location, None);
        assert_eq!(named.source, Source::Unnamed("900001".into()));
        assert_eq!(named.directory_error, None);
    }

    #[test]
    fn a_cache_that_does_not_read_is_reported_and_profiles_still_name_it() {
        let broken = || {
            Err(CliError::Other(
                "directory.json is not a directory cache".into(),
            ))
        };
        let cfg = config(&[("wide", wide()), ("clinic", at("900001", "Example Clinic"))]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", broken);
        assert_eq!(named.name, "Example Clinic");
        assert_eq!(named.source, Source::Profile("clinic".into()));
        assert!(named
            .directory_error
            .as_deref()
            .unwrap()
            .contains("not a directory cache"));

        let cfg = config(&[("wide", wide())]);
        let named = header_name(&cfg, "wide", "a-900001.E-1001", broken);
        assert_eq!(named.name, WIDE_NAME);
        assert_eq!(named.source, Source::Unnamed("900001".into()));
        assert!(named.directory_error.is_some());
    }

    #[test]
    fn a_practice_sign_in_and_the_sandbox_keep_their_own_name_unread() {
        let cfg = config(&[
            ("clinic", at("900001", "Example Clinic")),
            ("other", at("900002", "Other")),
        ]);
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
        let named = header_name(&cfg, "wide", "patient-1", unread);
        assert_eq!(named.source, Source::SignIn);
        assert_eq!(named.name, WIDE_NAME);
    }
}
