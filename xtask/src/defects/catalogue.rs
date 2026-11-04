//! Parse and validate the defect catalogue files. Reads only the catalogue directory, never git,
//! so the check is pure and runs anywhere (CI included).

use eyre::{Context as _, bail, eyre};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

/// Catalogue location, relative to the workspace root.
pub const CATALOGUE_DIR: &str = "docs/99-reference/defect-catalogue";

/// Body sections every class file must have, in this order.
pub const REQUIRED_SECTIONS: [&str; 4] =
    ["Pattern", "Why it is a defect", "Correct form", "Detection"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Fork,
    DataLoss,
    Dos,
    Stall,
    Other,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fork => "fork",
            Self::DataLoss => "data-loss",
            Self::Dos => "dos",
            Self::Stall => "stall",
            Self::Other => "other",
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassMeta {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub applies_when: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ClassFile {
    pub slug: String,
    pub meta: ClassMeta,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FoundBy {
    Review,
    Test,
    Testnet,
    Mainnet,
    Audit,
    Sweep,
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub class: String,
    pub site: String,
    pub found: String,
    pub found_by: FoundBy,
    pub fix: String,
    pub released_in: String,
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceLog {
    #[serde(default)]
    instance: Vec<Instance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub id: String,
    pub link: String,
    pub title: String,
    pub severity: String,
    pub applies_when: String,
}

#[derive(Debug, Default)]
pub struct Catalogue {
    pub classes: Vec<ClassFile>,
    pub instances: Vec<Instance>,
    pub index: Vec<IndexRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Testnet,
    Mainnet,
}

/// Parses a release tag such as `testnet-4.1.1` into its stream and version.
pub fn parse_release(tag: &str) -> Option<(Stream, (u64, u64, u64))> {
    let (stream, version) = tag.split_once('-')?;
    let stream = match stream {
        "testnet" => Stream::Testnet,
        "mainnet" => Stream::Mainnet,
        _ => return None,
    };
    let mut parts = version.split('.').map(|part| {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            part.parse::<u64>().ok()
        }
    });
    let version = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() {
        return None;
    }
    Some((stream, version))
}

/// Splits a class file into its TOML front matter and its markdown body.
pub fn split_front_matter(text: &str) -> eyre::Result<(&str, &str)> {
    let rest = text
        .strip_prefix("+++\n")
        .ok_or_else(|| eyre!("front matter must start with a `+++` line"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "+++" {
            return Ok((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("front matter has no closing `+++` line")
}

/// Parses the index table: the first table whose header starts with `| Id |`.
pub fn parse_index(readme: &str) -> eyre::Result<Vec<IndexRow>> {
    let mut lines = readme
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("| Id |"));
    if lines.next().is_none() {
        bail!("README has no index table (a header row that starts with `| Id |`)");
    }
    match lines.next() {
        Some(separator) if separator.trim_start().starts_with("|-") => {}
        _ => bail!("index table header has no `|---` separator row"),
    }
    lines
        .take_while(|l| l.trim_start().starts_with('|'))
        .map(parse_index_row)
        .collect()
}

fn parse_index_row(line: &str) -> eyre::Result<IndexRow> {
    let inner = line
        .trim()
        .strip_prefix('|')
        .and_then(|l| l.strip_suffix('|'))
        .ok_or_else(|| eyre!("malformed index row: {line}"))?;
    let cells: Vec<&str> = inner.split('|').map(str::trim).collect();
    let [id_cell, title, severity, applies_when] = cells[..] else {
        bail!("index row must have 4 cells, got {}: {line}", cells.len());
    };
    let (id, link) = id_cell
        .strip_prefix('[')
        .and_then(|c| c.strip_suffix(')'))
        .and_then(|c| c.split_once("]("))
        .ok_or_else(|| eyre!("index row id cell must be `[<id>](classes/<id>.md)`: {line}"))?;
    Ok(IndexRow {
        id: id.to_owned(),
        link: link.to_owned(),
        title: title.to_owned(),
        severity: severity.to_owned(),
        applies_when: applies_when.to_owned(),
    })
}

/// Loads every catalogue file that parses. Each file that fails to read or parse becomes a
/// problem string that starts with the file path.
pub fn load(dir: &Path) -> (Catalogue, Vec<String>) {
    let mut catalogue = Catalogue::default();
    let mut problems = Vec::new();

    let readme_path = dir.join("README.md");
    match fs::read_to_string(&readme_path)
        .wrap_err("cannot read")
        .and_then(|text| parse_index(&text))
    {
        Ok(index) => catalogue.index = index,
        Err(err) => problems.push(format!("{}: {err:#}", readme_path.display())),
    }

    let log_path = dir.join("instances.toml");
    match fs::read_to_string(&log_path)
        .wrap_err("cannot read")
        .and_then(|text| toml::from_str::<InstanceLog>(&text).wrap_err("invalid TOML"))
    {
        Ok(log) => catalogue.instances = log.instance,
        Err(err) => problems.push(format!("{}: {err:#}", log_path.display())),
    }

    let classes_dir = dir.join("classes");
    let mut class_paths = match fs::read_dir(&classes_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect::<Vec<_>>(),
        Err(err) => {
            problems.push(format!("{}: cannot read: {err}", classes_dir.display()));
            Vec::new()
        }
    };
    class_paths.sort();
    for path in class_paths {
        let slug = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parsed = fs::read_to_string(&path)
            .wrap_err("cannot read")
            .and_then(|text| {
                let (front, body) = split_front_matter(&text)?;
                let meta = toml::from_str::<ClassMeta>(front).wrap_err("invalid front matter")?;
                Ok(ClassFile {
                    slug,
                    meta,
                    body: body.to_owned(),
                })
            });
        match parsed {
            Ok(class) => catalogue.classes.push(class),
            Err(err) => problems.push(format!("{}: {err:#}", path.display())),
        }
    }

    (catalogue, problems)
}

fn is_kebab_case(slug: &str) -> bool {
    !slug.is_empty()
        && slug.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Section headings (`## ...`) of a markdown body, in order.
fn section_headings(body: &str) -> Vec<&str> {
    body.lines()
        .filter_map(|l| l.strip_prefix("## "))
        .map(str::trim)
        .collect()
}

fn validate_class(class: &ClassFile, problems: &mut Vec<String>) {
    let id = &class.slug;
    let meta = &class.meta;
    if !is_kebab_case(id) {
        problems.push(format!("class {id}: file name must be kebab-case"));
    }
    if meta.id != *id {
        problems.push(format!(
            "class {id}: front matter id {:?} must equal the file name",
            meta.id
        ));
    }
    if meta.title.trim().is_empty() {
        problems.push(format!("class {id}: title is empty"));
    }
    if meta.applies_when.trim().is_empty() {
        problems.push(format!("class {id}: applies_when is empty"));
    }
    let headings = section_headings(&class.body);
    let mut remaining = headings.iter();
    for section in REQUIRED_SECTIONS {
        if !remaining.any(|h| *h == section) {
            problems.push(format!(
                "class {id}: section `## {section}` is missing or out of order (required order: {})",
                REQUIRED_SECTIONS.join(", ")
            ));
            break;
        }
    }
    for pattern in &meta.patterns {
        if let Err(err) = regex::Regex::new(pattern) {
            problems.push(format!("class {id}: invalid regex {pattern:?}: {err}"));
        }
    }
    for glob in &meta.paths {
        if let Err(err) = glob::Pattern::new(glob) {
            problems.push(format!("class {id}: invalid paths glob {glob:?}: {err}"));
        }
    }
}

fn validate_instance(
    position: usize,
    instance: &Instance,
    class_ids: &BTreeSet<&str>,
    problems: &mut Vec<String>,
) {
    let at = format!("instance {} ({})", position + 1, instance.site);
    if !class_ids.contains(instance.class.as_str()) {
        problems.push(format!("{at}: unknown class {:?}", instance.class));
    }
    if instance.site.trim().is_empty() {
        problems.push(format!("{at}: site is empty"));
    }
    if instance.fix.trim().is_empty() {
        problems.push(format!("{at}: fix is empty"));
    }
    if chrono::NaiveDate::parse_from_str(&instance.found, "%Y-%m-%d").is_err() {
        problems.push(format!(
            "{at}: found {:?} is not YYYY-MM-DD",
            instance.found
        ));
    }
    if parse_release(&instance.released_in).is_none() {
        problems.push(format!(
            "{at}: released_in {:?} must be a release tag such as testnet-4.1.1",
            instance.released_in
        ));
    }
}

fn validate_index(catalogue: &Catalogue, problems: &mut Vec<String>) {
    let mut rows: BTreeMap<&str, &IndexRow> = BTreeMap::new();
    for row in &catalogue.index {
        if rows.insert(row.id.as_str(), row).is_some() {
            problems.push(format!("README index: duplicate row for {}", row.id));
        }
    }
    for class in &catalogue.classes {
        let id = class.slug.as_str();
        let Some(row) = rows.remove(id) else {
            problems.push(format!("README index: no row for class {id}"));
            continue;
        };
        let expected_link = format!("classes/{id}.md");
        let severity = class.meta.severity.to_string();
        let columns = [
            ("link", row.link.as_str(), expected_link.as_str()),
            ("title", row.title.as_str(), class.meta.title.as_str()),
            ("severity", row.severity.as_str(), severity.as_str()),
            (
                "applies_when",
                row.applies_when.as_str(),
                class.meta.applies_when.as_str(),
            ),
        ];
        for (column, found, expected) in columns {
            if found != expected {
                problems.push(format!(
                    "README index: {id} {column} is {found:?}, class file has {expected:?}"
                ));
            }
        }
    }
    for id in rows.keys() {
        problems.push(format!("README index: row {id} has no class file"));
    }
}

/// Cross-checks a loaded catalogue. Returns every problem, not only the first.
pub fn validate(catalogue: &Catalogue) -> Vec<String> {
    let mut problems = Vec::new();
    for class in &catalogue.classes {
        validate_class(class, &mut problems);
    }
    let class_ids: BTreeSet<&str> = catalogue.classes.iter().map(|c| c.slug.as_str()).collect();
    for (position, instance) in catalogue.instances.iter().enumerate() {
        validate_instance(position, instance, &class_ids, &mut problems);
    }
    validate_index(catalogue, &mut problems);
    problems
}

/// Loads and validates the catalogue in `dir`. An empty result means the catalogue is valid.
pub fn check(dir: &Path) -> Vec<String> {
    let (catalogue, mut problems) = load(dir);
    problems.extend(validate(&catalogue));
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::TempDirBuilder;
    use std::fs;

    const VALID_CLASS: &str = r#"+++
id = "uncapped-x"
title = "Uncapped X"
severity = "dos"
applies_when = "adds a peer HTTP read"
paths = ["crates/p2p/**"]
patterns = ['\.json\(\)']
+++

## Pattern

Bad code.

## Why it is a defect

Reasons.

## Correct form

Good code.

## Detection

Scan.
"#;

    const VALID_README: &str = "# Defect catalogue

| Id | Title | Severity | Applies when |
|----|-------|----------|--------------|
| [uncapped-x](classes/uncapped-x.md) | Uncapped X | dos | adds a peer HTTP read |

## Process
";

    const EMPTY_README: &str = "# Defect catalogue

| Id | Title | Severity | Applies when |
|----|-------|----------|--------------|

## Process
";

    const VALID_INSTANCE: &str = r##"[[instance]]
class = "uncapped-x"
site = "crates/p2p/src/a.rs::fetch"
found = "2026-10-13"
found_by = "review"
fix = "#1574"
released_in = "testnet-4.1.0"
"##;

    fn write_catalogue(dir: &Path, readme: &str, classes: &[(&str, &str)], instances: &str) {
        fs::create_dir_all(dir.join("classes")).unwrap();
        fs::write(dir.join("README.md"), readme).unwrap();
        fs::write(dir.join("instances.toml"), instances).unwrap();
        for (slug, text) in classes {
            fs::write(dir.join("classes").join(format!("{slug}.md")), text).unwrap();
        }
    }

    fn problems_for(readme: &str, classes: &[(&str, &str)], instances: &str) -> Vec<String> {
        let tmp = TempDirBuilder::new().build();
        write_catalogue(tmp.path(), readme, classes, instances);
        check(tmp.path())
    }

    fn assert_one_problem_contains(problems: &[String], needle: &str) {
        assert!(
            problems.iter().any(|p| p.contains(needle)),
            "expected a problem containing {needle:?}, got {problems:?}"
        );
    }

    #[test]
    fn check_passes_on_valid_catalogue() {
        let problems = problems_for(VALID_README, &[("uncapped-x", VALID_CLASS)], VALID_INSTANCE);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn check_passes_on_empty_catalogue() {
        let problems = problems_for(EMPTY_README, &[], "");
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn check_reports_bad_front_matter() {
        let problems = problems_for(VALID_README, &[("uncapped-x", "id = 1\n")], "");
        assert_one_problem_contains(&problems, "uncapped-x.md");
    }

    #[test]
    fn check_reports_id_slug_mismatch() {
        let readme = VALID_README.replace(
            "[uncapped-x](classes/uncapped-x.md)",
            "[other](classes/other.md)",
        );
        let problems = problems_for(&readme, &[("other", VALID_CLASS)], "");
        assert_one_problem_contains(&problems, "other");
    }

    #[test]
    fn check_reports_missing_section() {
        let class = VALID_CLASS.replace("## Correct form\n\nGood code.\n\n", "");
        let problems = problems_for(VALID_README, &[("uncapped-x", &class)], "");
        assert_one_problem_contains(&problems, "Correct form");
    }

    #[test]
    fn check_reports_bad_regex() {
        let class = VALID_CLASS.replace(r"patterns = ['\.json\(\)']", "patterns = ['(']");
        let problems = problems_for(VALID_README, &[("uncapped-x", &class)], "");
        assert_one_problem_contains(&problems, "regex");
    }

    #[test]
    fn check_reports_unknown_class_in_log() {
        let instances = VALID_INSTANCE.replace(r#"class = "uncapped-x""#, r#"class = "nope""#);
        let problems = problems_for(VALID_README, &[("uncapped-x", VALID_CLASS)], &instances);
        assert_one_problem_contains(&problems, "nope");
    }

    #[test]
    fn check_reports_bad_release_tag() {
        let instances = VALID_INSTANCE.replace("testnet-4.1.0", "mainnet-latest");
        let problems = problems_for(VALID_README, &[("uncapped-x", VALID_CLASS)], &instances);
        assert_one_problem_contains(&problems, "mainnet-latest");
    }

    #[test]
    fn check_reports_index_drift() {
        let readme = VALID_README.replace("| Uncapped X |", "| Different |");
        let problems = problems_for(&readme, &[("uncapped-x", VALID_CLASS)], "");
        assert_one_problem_contains(&problems, "title");
    }

    #[test]
    fn check_reports_missing_index_row() {
        let problems = problems_for(EMPTY_README, &[("uncapped-x", VALID_CLASS)], "");
        assert_one_problem_contains(&problems, "uncapped-x");
    }

    #[test]
    fn index_row_with_backticks_parses() {
        let readme =
            VALID_README.replace("adds a peer HTTP read |", "adds `reqwest` `.json()` call |");
        let rows = parse_index(&readme).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].applies_when, "adds `reqwest` `.json()` call");
    }

    #[test]
    fn parse_release_accepts_and_rejects() {
        assert_eq!(
            parse_release("testnet-4.1.1"),
            Some((Stream::Testnet, (4, 1, 1)))
        );
        assert_eq!(
            parse_release("mainnet-0.12.3"),
            Some((Stream::Mainnet, (0, 12, 3)))
        );
        assert_eq!(parse_release("mainnet-latest"), None);
        assert_eq!(parse_release("testnet-4.1"), None);
    }
}
