use std::cmp::Ordering;

use buck2_error::buck2_error;
use buck2_error::conversion::from_any_with_tag;

pub(super) fn is_valid_bzlmod_module_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut last = first;
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '-' || ch == '_')
        {
            return false;
        }
        last = ch;
    }
    last.is_ascii_lowercase() || last.is_ascii_digit()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BzlmodVersionIdentifier {
    Digits { number: u64, raw: String },
    Text(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BzlmodVersion {
    release: Vec<BzlmodVersionIdentifier>,
    prerelease: Vec<BzlmodVersionIdentifier>,
}

pub(super) fn bzlmod_version_cmp(a: &str, b: &str) -> buck2_error::Result<Ordering> {
    let a = parse_bzlmod_version(a)?;
    let b = parse_bzlmod_version(b)?;

    match (a.release.is_empty(), b.release.is_empty()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Greater),
        (false, true) => return Ok(Ordering::Less),
        (false, false) => {}
    }

    let release = bzlmod_identifier_lex_cmp(&a.release, &b.release);
    if release != Ordering::Equal {
        return Ok(release);
    }

    match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) => Ok(bzlmod_identifier_lex_cmp(&a.prerelease, &b.prerelease)),
    }
}

pub(super) fn parse_bzlmod_version(version: &str) -> buck2_error::Result<BzlmodVersion> {
    if version.is_empty() {
        return Ok(BzlmodVersion {
            release: Vec::new(),
            prerelease: Vec::new(),
        });
    }

    let (version, build) = version.split_once('+').unwrap_or((version, ""));
    if !build.is_empty()
        && !build
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid bzlmod version build metadata `{}`",
            build
        ));
    }

    let (release, prerelease) = version.split_once('-').unwrap_or((version, ""));
    if release.is_empty()
        || !release
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.')
    {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid bzlmod version release `{}`",
            release
        ));
    }
    if !prerelease.is_empty()
        && !prerelease
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid bzlmod version prerelease `{}`",
            prerelease
        ));
    }

    Ok(BzlmodVersion {
        release: parse_bzlmod_version_identifiers(release)?,
        prerelease: if prerelease.is_empty() {
            Vec::new()
        } else {
            parse_bzlmod_version_identifiers(prerelease)?
        },
    })
}

fn parse_bzlmod_version_identifiers(
    value: &str,
) -> buck2_error::Result<Vec<BzlmodVersionIdentifier>> {
    value
        .split('.')
        .map(|identifier| {
            if identifier.is_empty() {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "empty bzlmod version identifier in `{}`",
                    value
                ));
            }
            if identifier.chars().all(|ch| ch.is_ascii_digit()) {
                let number = identifier.parse::<u64>().map_err(|e| {
                    from_any_with_tag(e, buck2_error::ErrorTag::Input).context(format!(
                        "numeric bzlmod version identifier `{identifier}` is too large"
                    ))
                })?;
                Ok(BzlmodVersionIdentifier::Digits {
                    number,
                    raw: identifier.to_owned(),
                })
            } else {
                Ok(BzlmodVersionIdentifier::Text(identifier.to_owned()))
            }
        })
        .collect()
}

fn bzlmod_identifier_lex_cmp(
    a: &[BzlmodVersionIdentifier],
    b: &[BzlmodVersionIdentifier],
) -> Ordering {
    for (a, b) in a.iter().zip(b) {
        let cmp = bzlmod_identifier_cmp(a, b);
        if cmp != Ordering::Equal {
            return cmp;
        }
    }
    a.len().cmp(&b.len())
}

fn bzlmod_identifier_cmp(a: &BzlmodVersionIdentifier, b: &BzlmodVersionIdentifier) -> Ordering {
    match (a, b) {
        (
            BzlmodVersionIdentifier::Digits {
                number: a_number,
                raw: a_raw,
            },
            BzlmodVersionIdentifier::Digits {
                number: b_number,
                raw: b_raw,
            },
        ) => a_number.cmp(b_number).then_with(|| a_raw.cmp(b_raw)),
        (BzlmodVersionIdentifier::Digits { .. }, BzlmodVersionIdentifier::Text(_)) => {
            Ordering::Less
        }
        (BzlmodVersionIdentifier::Text(_), BzlmodVersionIdentifier::Digits { .. }) => {
            Ordering::Greater
        }
        (BzlmodVersionIdentifier::Text(a), BzlmodVersionIdentifier::Text(b)) => a.cmp(b),
    }
}
