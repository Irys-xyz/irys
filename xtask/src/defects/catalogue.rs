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

/// The detection list: one line per class, read first in a review.
pub const DETECT_FILE: &str = "detect.md";

/// The section of the detection list that holds the class lines.
const DETECT_CLASSES_HEADING: &str = "## Classes";

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
    pub severity: Severity,
    /// The code this class can occur in.
    pub scope: String,
    /// What to flag: the defect signal, in code terms.
    pub flag: String,
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

#[derive(Debug, Default)]
pub struct Catalogue {
    pub classes: Vec<ClassFile>,
    pub instances: Vec<Instance>,
    /// The class lines of the detection list, in file order.
    pub detect_lines: Vec<String>,
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

/// The detection-list line for a class. `check` requires `detect.md` to hold exactly this line.
pub fn detect_line(meta: &ClassMeta) -> String {
    format!(
        "- `{}` ({}). Scope: {}. Flag: {}.",
        meta.id, meta.severity, meta.scope, meta.flag
    )
}

/// The class lines of a detection list: the `- ` lines under its `## Classes` heading.
pub fn parse_detect(text: &str) -> eyre::Result<Vec<String>> {
    let mut lines = text
        .lines()
        .skip_while(|l| l.trim_end() != DETECT_CLASSES_HEADING);
    if lines.next().is_none() {
        bail!("no `{DETECT_CLASSES_HEADING}` heading");
    }
    Ok(lines
        .take_while(|l| !l.starts_with("## "))
        .filter(|l| l.starts_with("- "))
        .map(|l| l.trim_end().to_owned())
        .collect())
}

/// The class id of a detection line: the text in its first pair of backticks.
fn detect_line_id(line: &str) -> Option<&str> {
    line.strip_prefix("- `")?.split_once('`').map(|(id, _)| id)
}

/// Loads every catalogue file that parses. Each file that fails to read or parse becomes a
/// problem string that starts with the file path.
pub fn load(dir: &Path) -> (Catalogue, Vec<String>) {
    let mut catalogue = Catalogue::default();
    let mut problems = Vec::new();

    let detect_path = dir.join(DETECT_FILE);
    match fs::read_to_string(&detect_path)
        .wrap_err("cannot read")
        .and_then(|text| parse_detect(&text))
    {
        Ok(lines) => catalogue.detect_lines = lines,
        Err(err) => problems.push(format!("{}: {err:#}", detect_path.display())),
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
    for (field, text) in [("scope", &meta.scope), ("flag", &meta.flag)] {
        if text.trim().is_empty() || text.contains('\n') || text.trim_end().ends_with('.') {
            problems.push(format!(
                "class {id}: {field} must be one non-empty line with no final `.`"
            ));
        }
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

fn validate_detect(catalogue: &Catalogue, problems: &mut Vec<String>) {
    let mut lines: BTreeMap<&str, &str> = BTreeMap::new();
    for line in &catalogue.detect_lines {
        let Some(id) = detect_line_id(line) else {
            problems.push(format!(
                "{DETECT_FILE}: line must start with \"- `<id>`\": {line}"
            ));
            continue;
        };
        if lines.insert(id, line).is_some() {
            problems.push(format!("{DETECT_FILE}: duplicate line for {id}"));
        }
    }
    for class in &catalogue.classes {
        let id = class.slug.as_str();
        let expected = detect_line(&class.meta);
        match lines.remove(id) {
            None => problems.push(format!(
                "{DETECT_FILE}: no line for class {id}; add: {expected}"
            )),
            Some(line) if line != expected => problems.push(format!(
                "{DETECT_FILE}: line for {id} differs from its class file; expected: {expected}"
            )),
            Some(_) => {}
        }
    }
    for id in lines.keys() {
        problems.push(format!("{DETECT_FILE}: line {id} has no class file"));
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
    validate_detect(catalogue, &mut problems);
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
severity = "dos"
scope = "code that reads a peer HTTP body"
flag = "`.json()` with no byte cap"
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

    const VALID_LINE: &str = "- `uncapped-x` (dos). Scope: code that reads a peer HTTP body. Flag: `.json()` with no byte cap.";

    fn detect_with(lines: &[&str]) -> String {
        format!(
            "# Detection list\n\nIntro.\n\n## Classes\n\n{}\n\n## Do not flag\n\n- `RethService` exits on a mismatch: intended.\n",
            lines.join("\n")
        )
    }

    const VALID_INSTANCE: &str = r##"[[instance]]
class = "uncapped-x"
site = "crates/p2p/src/a.rs::fetch"
found = "2026-10-13"
found_by = "review"
fix = "#1574"
released_in = "testnet-4.1.0"
"##;

    fn write_catalogue(dir: &Path, detect: &str, classes: &[(&str, &str)], instances: &str) {
        fs::create_dir_all(dir.join("classes")).unwrap();
        fs::write(dir.join(DETECT_FILE), detect).unwrap();
        fs::write(dir.join("instances.toml"), instances).unwrap();
        for (slug, text) in classes {
            fs::write(dir.join("classes").join(format!("{slug}.md")), text).unwrap();
        }
    }

    fn problems_for(detect: &str, classes: &[(&str, &str)], instances: &str) -> Vec<String> {
        let tmp = TempDirBuilder::new().build();
        write_catalogue(tmp.path(), detect, classes, instances);
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
        let problems = problems_for(
            &detect_with(&[VALID_LINE]),
            &[("uncapped-x", VALID_CLASS)],
            VALID_INSTANCE,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn check_passes_on_empty_catalogue() {
        let problems = problems_for(&detect_with(&[]), &[], "");
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn check_reports_bad_front_matter() {
        let problems = problems_for(&detect_with(&[]), &[("uncapped-x", "id = 1\n")], "");
        assert_one_problem_contains(&problems, "uncapped-x.md");
    }

    #[test]
    fn check_reports_id_slug_mismatch() {
        let line = VALID_LINE.replace("`uncapped-x`", "`other`");
        let problems = problems_for(&detect_with(&[&line]), &[("other", VALID_CLASS)], "");
        assert_one_problem_contains(&problems, "must equal the file name");
    }

    #[test]
    fn check_reports_missing_section() {
        let class = VALID_CLASS.replace("## Correct form\n\nGood code.\n\n", "");
        let problems = problems_for(&detect_with(&[VALID_LINE]), &[("uncapped-x", &class)], "");
        assert_one_problem_contains(&problems, "Correct form");
    }

    #[test]
    fn check_reports_bad_regex() {
        let class = VALID_CLASS.replace(r"patterns = ['\.json\(\)']", "patterns = ['(']");
        let problems = problems_for(&detect_with(&[VALID_LINE]), &[("uncapped-x", &class)], "");
        assert_one_problem_contains(&problems, "regex");
    }

    #[test]
    fn check_reports_unknown_class_in_log() {
        let instances = VALID_INSTANCE.replace(r#"class = "uncapped-x""#, r#"class = "nope""#);
        let problems = problems_for(
            &detect_with(&[VALID_LINE]),
            &[("uncapped-x", VALID_CLASS)],
            &instances,
        );
        assert_one_problem_contains(&problems, "nope");
    }

    #[test]
    fn check_reports_bad_release_tag() {
        let instances = VALID_INSTANCE.replace("testnet-4.1.0", "mainnet-latest");
        let problems = problems_for(
            &detect_with(&[VALID_LINE]),
            &[("uncapped-x", VALID_CLASS)],
            &instances,
        );
        assert_one_problem_contains(&problems, "mainnet-latest");
    }

    #[test]
    fn check_reports_detect_line_drift_once_with_expected_text() {
        let line = VALID_LINE.replace("no byte cap", "no cap");
        let problems = problems_for(&detect_with(&[&line]), &[("uncapped-x", VALID_CLASS)], "");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "differs");
        assert_one_problem_contains(&problems, VALID_LINE);
    }

    #[test]
    fn check_reports_missing_detect_line_with_expected_text() {
        let problems = problems_for(&detect_with(&[]), &[("uncapped-x", VALID_CLASS)], "");
        assert_one_problem_contains(&problems, "no line for class uncapped-x");
        assert_one_problem_contains(&problems, VALID_LINE);
    }

    #[test]
    fn check_reports_detect_line_with_no_class() {
        let problems = problems_for(&detect_with(&[VALID_LINE]), &[], "");
        assert_one_problem_contains(&problems, "no class file");
    }

    #[test]
    fn check_reports_duplicate_detect_line() {
        let problems = problems_for(
            &detect_with(&[VALID_LINE, VALID_LINE]),
            &[("uncapped-x", VALID_CLASS)],
            "",
        );
        assert_one_problem_contains(&problems, "duplicate");
    }

    #[test]
    fn check_reports_detect_file_without_classes_section() {
        let problems = problems_for("# Detection list\n", &[], "");
        assert_one_problem_contains(&problems, "## Classes");
    }

    #[test]
    fn check_rejects_flag_that_ends_with_period_or_spans_lines() {
        for bad in ["`.json()` with no byte cap.", "two\\nlines"] {
            let class = VALID_CLASS.replace("`.json()` with no byte cap", bad);
            let problems = problems_for(&detect_with(&[]), &[("uncapped-x", &class)], "");
            assert_one_problem_contains(&problems, "flag");
        }
    }

    #[test]
    fn detect_line_ignores_do_not_flag_section() {
        let lines = parse_detect(&detect_with(&[VALID_LINE])).unwrap();
        assert_eq!(lines, vec![VALID_LINE.to_owned()]);
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
