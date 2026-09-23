//! The builder refuses tenant input Sōzu would reject by making the calls
//! Sōzu makes: `regex::bytes::Regex::new` for path rules and
//! `idna::domain_to_ascii` for hostnames. That only holds while those crates,
//! and the Unicode data behind IDNA, resolve to the versions Sōzu 2.2.1 builds
//! against. Most of them are held by `Cargo.lock` alone, which a routine
//! `cargo update` moves: a newer grammar or Unicode table could then admit a
//! name Sōzu 2.2.1 still refuses, and one such name fails every reconcile of
//! the shared instance. This test reads the lock and fails on that drift.
//!
//! Only the versions the builder itself links are checked: the lock's
//! dependency edges are followed from the builder's own `regex` and `idna`
//! entries, so a second, unrelated copy of one of these crates pulled in by
//! another dependency is not drift.

use std::collections::{BTreeSet, HashMap};

/// The package whose validation dependencies are checked.
const BUILDER: &str = "sozu-gw-builder";

/// The builder's direct dependencies that carry Sōzu's validation calls.
const ROOTS: &[&str] = &["regex", "idna"];

/// `(crate, version)` as resolved by Sōzu 2.2.1's own `Cargo.lock`. Move an
/// entry only together with the Sōzu version the chart deploys, and re-verify
/// the builder's refusals against that release.
const SOZU_2_2_1: &[(&str, &str)] = &[
    // Path rules (`admit_path`).
    ("regex", "1.13.1"),
    ("regex-syntax", "0.8.11"),
    ("regex-automata", "0.4.18"),
    // Hostnames (`admit_hostname`): IDNA and its UTS #46 / normalization data.
    ("idna", "1.1.0"),
    ("idna_adapter", "1.2.2"),
    ("icu_normalizer", "2.2.0"),
    ("icu_normalizer_data", "2.2.0"),
    ("icu_properties", "2.2.0"),
    ("icu_properties_data", "2.2.0"),
    ("icu_collections", "2.2.0"),
];

/// One `[[package]]` of the lock: its version and its dependency entries, as
/// written (`"name"`, or `"name version"` / `"name version (source)"` when the
/// lock holds more than one version of that name).
struct Package<'a> {
    name: &'a str,
    version: &'a str,
    deps: Vec<&'a str>,
}

fn parse(lock: &str) -> Vec<Package<'_>> {
    let mut packages = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let mut name = None;
        let mut version = None;
        let mut deps = Vec::new();
        let mut in_deps = false;
        for line in block.lines() {
            let line = line.trim();
            if in_deps {
                if line == "]" {
                    in_deps = false;
                } else {
                    deps.push(line.trim_end_matches(',').trim_matches('"'));
                }
            } else if let Some(v) = line.strip_prefix("name = ") {
                name = Some(v.trim_matches('"'));
            } else if let Some(v) = line.strip_prefix("version = ") {
                version = Some(v.trim_matches('"'));
            } else if line == "dependencies = [" {
                in_deps = true;
            }
        }
        if let (Some(name), Some(version)) = (name, version) {
            packages.push(Package {
                name,
                version,
                deps,
            });
        }
    }
    packages
}

/// Index of the package a dependency entry designates. A bare name is only
/// written when the lock holds a single version of it.
fn resolve(packages: &[Package<'_>], entry: &str) -> Option<usize> {
    let mut parts = entry.split_whitespace();
    let name = parts.next()?;
    let version = parts.next();
    let mut hits = packages
        .iter()
        .enumerate()
        .filter(|(_, p)| p.name == name && version.is_none_or(|v| p.version == v));
    let (idx, _) = hits.next()?;
    hits.next().is_none().then_some(idx)
}

/// For each pinned crate, the versions the builder's validation roots reach
/// through the lock's dependency edges, and a line per crate whose reached
/// versions are not exactly Sōzu's.
fn drift(lock: &str) -> Vec<String> {
    let packages = parse(lock);
    let Some(builder) = packages.iter().position(|p| p.name == BUILDER) else {
        return vec![format!("{BUILDER}: not in the lock")];
    };

    let mut problems = Vec::new();
    let mut stack = Vec::new();
    for &root in ROOTS {
        let entry = packages[builder]
            .deps
            .iter()
            .find(|d| d.split_whitespace().next() == Some(root));
        match entry.and_then(|e| resolve(&packages, e)) {
            Some(idx) => stack.push(idx),
            None => problems.push(format!("{BUILDER}: no resolved dependency on {root}")),
        }
    }

    let mut seen = BTreeSet::new();
    while let Some(idx) = stack.pop() {
        if !seen.insert(idx) {
            continue;
        }
        for entry in &packages[idx].deps {
            match resolve(&packages, entry) {
                Some(dep) => stack.push(dep),
                None => problems.push(format!(
                    "{}: dependency {entry:?} resolves to no single package",
                    packages[idx].name
                )),
            }
        }
    }

    let mut reached: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for &idx in &seen {
        reached
            .entry(packages[idx].name)
            .or_default()
            .insert(packages[idx].version);
    }

    problems.extend(SOZU_2_2_1.iter().filter_map(|&(name, want)| {
        let got: Vec<&str> = reached
            .get(name)
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();
        (got != [want]).then(|| format!("{name}: builder links {got:?}, Sōzu 2.2.1 uses {want}"))
    }));
    problems
}

#[test]
fn validation_crates_resolve_to_the_versions_sozu_builds_against() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock");
    let lock = std::fs::read_to_string(path).expect("read the workspace Cargo.lock");

    let drift = drift(&lock);
    assert!(
        drift.is_empty(),
        "the builder would no longer judge input the way Sōzu 2.2.1 does; \
         restore with `cargo update -p <crate> --precise <version>`:\n{}",
        drift.join("\n")
    );
}

/// A lock shaped like the real one around the pinned crates. `extra` is
/// appended as further `[[package]]` blocks; `{syntax}` is the entry the
/// builder's `regex` and `regex-automata` use for `regex-syntax`.
fn fixture(syntax: &str, extra: &str) -> String {
    let lock = r#"
version = 4

[[package]]
name = "sozu-gw-builder"
version = "0.1.0"
dependencies = [
 "idna",
 "regex",
 "unrelated",
]

[[package]]
name = "regex"
version = "1.13.1"
dependencies = [
 "regex-automata",
 "{syntax}",
]

[[package]]
name = "regex-automata"
version = "0.4.18"
dependencies = [
 "{syntax}",
]

[[package]]
name = "regex-syntax"
version = "0.8.11"

[[package]]
name = "idna"
version = "1.1.0"
dependencies = [
 "idna_adapter",
]

[[package]]
name = "idna_adapter"
version = "1.2.2"
dependencies = [
 "icu_normalizer",
 "icu_properties",
]

[[package]]
name = "icu_normalizer"
version = "2.2.0"
dependencies = [
 "icu_collections",
 "icu_normalizer_data",
 "icu_properties",
]

[[package]]
name = "icu_normalizer_data"
version = "2.2.0"

[[package]]
name = "icu_properties"
version = "2.2.0"
dependencies = [
 "icu_collections",
 "icu_properties_data",
]

[[package]]
name = "icu_properties_data"
version = "2.2.0"

[[package]]
name = "icu_collections"
version = "2.2.0"
"#;
    format!("{}{extra}", lock.replace("{syntax}", syntax))
}

#[test]
fn a_lock_matching_sozu_passes() {
    assert_eq!(drift(&fixture("regex-syntax", "")), Vec::<String>::new());
}

#[test]
fn a_second_version_used_only_elsewhere_is_not_drift() {
    let lock = fixture(
        "regex-syntax 0.8.11",
        r#"
[[package]]
name = "regex-syntax"
version = "0.7.5"

[[package]]
name = "unrelated"
version = "1.0.0"
dependencies = [
 "regex-syntax 0.7.5",
]
"#,
    );
    assert_eq!(drift(&lock), Vec::<String>::new());
}

#[test]
fn a_drift_of_the_version_the_builder_links_fails() {
    // The builder's chain moves to 0.8.12 while 0.8.11 survives elsewhere.
    let lock = fixture("regex-syntax 0.8.12", "").replace(
        "name = \"regex-syntax\"\nversion = \"0.8.11\"",
        "name = \"regex-syntax\"\nversion = \"0.8.12\"",
    ) + r#"
[[package]]
name = "regex-syntax"
version = "0.8.11"

[[package]]
name = "unrelated"
version = "1.0.0"
dependencies = [
 "regex-syntax 0.8.11",
]
"#;
    assert_eq!(
        drift(&lock),
        ["regex-syntax: builder links [\"0.8.12\"], Sōzu 2.2.1 uses 0.8.11"]
    );
}

#[test]
fn a_crate_that_leaves_the_builder_chain_fails() {
    // idna stops depending on idna_adapter: its Unicode data is no longer
    // the one checked, so every crate below it reads as missing.
    let lock = fixture("regex-syntax", "").replace(
        "name = \"idna\"\nversion = \"1.1.0\"\ndependencies = [\n \"idna_adapter\",\n]",
        "name = \"idna\"\nversion = \"1.1.0\"",
    );
    let drift = drift(&lock);
    assert!(
        drift
            .iter()
            .any(|d| d == "idna_adapter: builder links [], Sōzu 2.2.1 uses 1.2.2"),
        "{drift:?}"
    );
}
