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

/// The section of the detection list that holds one line per known non-issue.
const DETECT_DO_NOT_FLAG_HEADING: &str = "## Do not flag";

/// The catalogue README: the process, and the full entry for each known non-issue.
pub const README_FILE: &str = "README.md";

/// The README section with one `### <id>` entry per known non-issue.
const README_NON_ISSUES_HEADING: &str = "## Known non-issues";

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

/// How a class is stopped today: the strongest control that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Enforcement {
    /// Only a reviewer finds it.
    Review,
    /// `defects scan` patterns give leads; a reviewer confirms them.
    Scan,
    /// A lint or another CI check rejects the code.
    Lint,
    /// A type or an API makes the defect impossible to write.
    Type,
}

impl fmt::Display for Enforcement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Review => "review",
            Self::Scan => "scan",
            Self::Lint => "lint",
            Self::Type => "type",
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassMeta {
    pub id: String,
    pub severity: Severity,
    pub enforcement: Enforcement,
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
    /// The "Do not flag" lines of the detection list, in file order.
    pub do_not_flag_lines: Vec<String>,
    /// The ids of the known non-issues in the README, in file order.
    pub known_non_issues: Vec<String>,
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

/// The lines of the markdown section under `heading`, up to the next `## ` heading. `None` if
/// the text has no such heading.
fn section_lines<'a>(text: &'a str, heading: &str) -> Option<impl Iterator<Item = &'a str>> {
    let mut lines = text.lines().skip_while(move |l| l.trim_end() != heading);
    lines.next()?;
    Some(
        lines
            .take_while(|l| !l.starts_with("## "))
            .map(str::trim_end),
    )
}

fn bullet_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    lines
        .filter(|l| l.starts_with("- "))
        .map(str::to_owned)
        .collect()
}

/// The class lines of a detection list: the `- ` lines under its `## Classes` heading.
pub fn parse_detect(text: &str) -> eyre::Result<Vec<String>> {
    section_lines(text, DETECT_CLASSES_HEADING)
        .map(bullet_lines)
        .ok_or_else(|| eyre!("no `{DETECT_CLASSES_HEADING}` heading"))
}

/// The "Do not flag" lines of a detection list. Empty if the section is missing.
fn parse_do_not_flag(text: &str) -> Vec<String> {
    section_lines(text, DETECT_DO_NOT_FLAG_HEADING)
        .map(bullet_lines)
        .unwrap_or_default()
}

/// The known non-issue ids of the README: its `### <id>` headings under "Known non-issues".
/// Empty if the section is missing.
fn parse_known_non_issues(text: &str) -> Vec<String> {
    section_lines(text, README_NON_ISSUES_HEADING)
        .map(|lines| {
            lines
                .filter_map(|l| l.strip_prefix("### "))
                .map(|id| id.trim().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The id of a detection line: the text in its first pair of backticks.
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
        .and_then(|text| Ok((parse_detect(&text)?, parse_do_not_flag(&text))))
    {
        Ok((lines, do_not_flag)) => {
            catalogue.detect_lines = lines;
            catalogue.do_not_flag_lines = do_not_flag;
        }
        Err(err) => problems.push(format!("{}: {err:#}", detect_path.display())),
    }

    let readme_path = dir.join(README_FILE);
    match fs::read_to_string(&readme_path) {
        Ok(text) => catalogue.known_non_issues = parse_known_non_issues(&text),
        Err(err) => problems.push(format!("{}: cannot read: {err}", readme_path.display())),
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

/// A `scope` or `flag` value: one non-empty line with no final `.`.
fn is_detect_text(text: &str) -> bool {
    !text.trim().is_empty() && !text.contains('\n') && !text.trim_end().ends_with('.')
}

/// A `fix` value: a PR number (`#1678`) or a commit SHA (7 to 40 hex digits).
fn is_fix_reference(fix: &str) -> bool {
    match fix.strip_prefix('#') {
        Some(number) => !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
        None => (7..=40).contains(&fix.len()) && fix.bytes().all(|b| b.is_ascii_hexdigit()),
    }
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
        if !is_detect_text(text) {
            problems.push(format!(
                "class {id}: {field} {text:?} must be one non-empty line with no final `.` (the {DETECT_FILE} line adds it)"
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
    match (meta.enforcement, meta.patterns.is_empty()) {
        (Enforcement::Scan, true) => problems.push(format!(
            "class {id}: enforcement \"scan\" needs at least one pattern"
        )),
        (Enforcement::Review, false) => problems.push(format!(
            "class {id}: has scan patterns, so enforcement must be \"scan\", \"lint\" or \"type\", not \"review\""
        )),
        _ => {}
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
    if !is_fix_reference(&instance.fix) {
        problems.push(format!(
            "{at}: fix {:?} must be a PR number such as #1678 or a commit SHA",
            instance.fix
        ));
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

/// The lines of one detection-list section, by id. Reports lines with no id and duplicate ids.
fn index_detect_lines<'a>(
    lines: &'a [String],
    section: &str,
    problems: &mut Vec<String>,
) -> BTreeMap<&'a str, &'a str> {
    let mut by_id = BTreeMap::new();
    for line in lines {
        let Some(id) = detect_line_id(line) else {
            problems.push(format!(
                "{DETECT_FILE}: {section} line must start with \"- `<id>`\": {line}"
            ));
            continue;
        };
        if by_id.insert(id, line.as_str()).is_some() {
            problems.push(format!("{DETECT_FILE}: duplicate {section} line for {id}"));
        }
    }
    by_id
}

fn validate_detect(catalogue: &Catalogue, problems: &mut Vec<String>) {
    let mut lines = index_detect_lines(&catalogue.detect_lines, "class", problems);
    for class in &catalogue.classes {
        let id = class.slug.as_str();
        let line = lines.remove(id);
        if !is_detect_text(&class.meta.scope) || !is_detect_text(&class.meta.flag) {
            // `validate_class` reports the field; a line built from it would be wrong too.
            continue;
        }
        let expected = detect_line(&class.meta);
        match line {
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

/// Each known non-issue in the README has one "Do not flag" line in the detection list, and
/// each "Do not flag" line has a README entry.
fn validate_known_non_issues(catalogue: &Catalogue, problems: &mut Vec<String>) {
    let mut lines = index_detect_lines(&catalogue.do_not_flag_lines, "\"Do not flag\"", problems);
    let mut seen = BTreeSet::new();
    for id in &catalogue.known_non_issues {
        if !is_kebab_case(id) {
            problems.push(format!(
                "{README_FILE}: known non-issue heading {id:?} must be a kebab-case id"
            ));
        }
        if !seen.insert(id.as_str()) {
            problems.push(format!("{README_FILE}: duplicate known non-issue {id}"));
            continue;
        }
        if lines.remove(id.as_str()).is_none() {
            problems.push(format!(
                "{DETECT_FILE}: no \"Do not flag\" line for known non-issue {id}; add: - `{id}`: <what not to flag>."
            ));
        }
    }
    for id in lines.keys() {
        problems.push(format!(
            "{DETECT_FILE}: \"Do not flag\" line {id} has no entry under \"Known non-issues\" in {README_FILE}"
        ));
    }
}

/// Cross-checks a loaded catalogue. Returns every problem, not only the first.
pub fn validate(catalogue: &Catalogue) -> Vec<String> {
    let mut problems = Vec::new();
    for class in &catalogue.classes {
        validate_class(class, &mut problems);
    }
    let class_ids: BTreeSet<&str> = catalogue.classes.iter().map(|c| c.slug.as_str()).collect();
    // One fix removes a site once, so a second row with the same class, site and fix would
    // count the same instance twice in the trend.
    let mut first_row: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for (position, instance) in catalogue.instances.iter().enumerate() {
        validate_instance(position, instance, &class_ids, &mut problems);
        let key = (
            instance.class.as_str(),
            instance.site.as_str(),
            instance.fix.as_str(),
        );
        let first = *first_row.entry(key).or_insert(position);
        if first != position {
            problems.push(format!(
                "instance {} ({}): same class, site and fix as instance {}",
                position + 1,
                instance.site,
                first + 1
            ));
        }
    }
    validate_detect(catalogue, &mut problems);
    validate_known_non_issues(catalogue, &mut problems);
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
enforcement = "scan"
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

    const VALID_DO_NOT_FLAG_LINE: &str =
        "- `reth-exit`: `RethService` exits on a mismatch. This is intended.";

    fn detect_with(lines: &[&str]) -> String {
        detect_with_do_not_flag(lines, &[VALID_DO_NOT_FLAG_LINE])
    }

    fn detect_with_do_not_flag(lines: &[&str], do_not_flag: &[&str]) -> String {
        format!(
            "# Detection list\n\nIntro.\n\n## Classes\n\n{}\n\n## Do not flag\n\n{}\n",
            lines.join("\n"),
            do_not_flag.join("\n")
        )
    }

    fn readme_with(non_issue_ids: &[&str]) -> String {
        let entries: String = non_issue_ids
            .iter()
            .map(|id| format!("### {id}\n\nWhy it is correct.\n\n"))
            .collect();
        format!(
            "# Defect catalogue\n\n## Process\n\n### Not an entry\n\n## Known non-issues\n\n{entries}## Later\n\n### also-not-an-entry\n"
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

    fn write_catalogue(
        dir: &Path,
        readme: &str,
        detect: &str,
        classes: &[(&str, &str)],
        instances: &str,
    ) {
        fs::create_dir_all(dir.join("classes")).unwrap();
        fs::write(dir.join(README_FILE), readme).unwrap();
        fs::write(dir.join(DETECT_FILE), detect).unwrap();
        fs::write(dir.join("instances.toml"), instances).unwrap();
        for (slug, text) in classes {
            fs::write(dir.join("classes").join(format!("{slug}.md")), text).unwrap();
        }
    }

    fn problems_for(detect: &str, classes: &[(&str, &str)], instances: &str) -> Vec<String> {
        problems_with_readme(&readme_with(&["reth-exit"]), detect, classes, instances)
    }

    fn problems_with_readme(
        readme: &str,
        detect: &str,
        classes: &[(&str, &str)],
        instances: &str,
    ) -> Vec<String> {
        let tmp = TempDirBuilder::new().build();
        write_catalogue(tmp.path(), readme, detect, classes, instances);
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
    fn check_rejects_fix_that_is_not_pr_or_sha() {
        for bad in ["later", "#", "#12a", "abc12", "g1234567"] {
            let instances = VALID_INSTANCE.replace(r##"fix = "#1574""##, &format!("fix = {bad:?}"));
            let problems = problems_for(
                &detect_with(&[VALID_LINE]),
                &[("uncapped-x", VALID_CLASS)],
                &instances,
            );
            assert_one_problem_contains(&problems, "fix");
        }
        for good in ["#1", "abc1234", "0123456789abcdef0123456789abcdef01234567"] {
            let instances =
                VALID_INSTANCE.replace(r##"fix = "#1574""##, &format!("fix = {good:?}"));
            let problems = problems_for(
                &detect_with(&[VALID_LINE]),
                &[("uncapped-x", VALID_CLASS)],
                &instances,
            );
            assert!(problems.is_empty(), "{good}: {problems:?}");
        }
    }

    #[test]
    fn flag_with_final_period_reports_one_problem_that_quotes_it() {
        let class = VALID_CLASS.replace("no byte cap\"", "no byte cap.\"");
        let line = VALID_LINE.replace("cap.", "cap..");
        let problems = problems_for(&detect_with(&[&line]), &[("uncapped-x", &class)], "");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "no byte cap.\"");
    }

    #[test]
    fn detect_line_ignores_do_not_flag_section() {
        let lines = parse_detect(&detect_with(&[VALID_LINE])).unwrap();
        assert_eq!(lines, vec![VALID_LINE.to_owned()]);
    }

    #[test]
    fn check_rejects_unknown_enforcement() {
        let class = VALID_CLASS.replace(r#"enforcement = "scan""#, r#"enforcement = "hope""#);
        let problems = problems_for(&detect_with(&[VALID_LINE]), &[("uncapped-x", &class)], "");
        assert_one_problem_contains(&problems, "uncapped-x.md");
    }

    #[test]
    fn check_rejects_scan_enforcement_with_no_patterns() {
        let class = VALID_CLASS.replace("patterns = ['\\.json\\(\\)']\n", "");
        assert!(!class.contains("patterns"));
        let problems = problems_for(&detect_with(&[VALID_LINE]), &[("uncapped-x", &class)], "");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "enforcement \"scan\" needs at least one pattern");
    }

    #[test]
    fn check_rejects_review_enforcement_with_patterns() {
        let class = VALID_CLASS.replace(r#"enforcement = "scan""#, r#"enforcement = "review""#);
        let problems = problems_for(&detect_with(&[VALID_LINE]), &[("uncapped-x", &class)], "");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "has scan patterns");
    }

    #[test]
    fn check_accepts_lint_and_type_enforcement_with_or_without_patterns() {
        for value in ["lint", "type"] {
            let with_patterns = VALID_CLASS.replace(
                r#"enforcement = "scan""#,
                &format!("enforcement = {value:?}"),
            );
            let without_patterns = with_patterns.replace("patterns = ['\\.json\\(\\)']\n", "");
            for class in [with_patterns, without_patterns] {
                let problems =
                    problems_for(&detect_with(&[VALID_LINE]), &[("uncapped-x", &class)], "");
                assert!(problems.is_empty(), "{value}: {problems:?}");
            }
        }
    }

    #[test]
    fn check_reports_known_non_issue_with_no_do_not_flag_line() {
        let problems = problems_with_readme(
            &readme_with(&["reth-exit", "slow-path"]),
            &detect_with(&[]),
            &[],
            "",
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "slow-path");
        assert_one_problem_contains(&problems, "no \"Do not flag\" line");
    }

    #[test]
    fn check_reports_do_not_flag_line_with_no_known_non_issue() {
        let problems = problems_with_readme(&readme_with(&[]), &detect_with(&[]), &[], "");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "reth-exit");
        assert_one_problem_contains(&problems, "Known non-issues");
    }

    #[test]
    fn check_reports_duplicate_and_malformed_do_not_flag_lines() {
        let problems = problems_for(
            &detect_with_do_not_flag(
                &[],
                &[
                    VALID_DO_NOT_FLAG_LINE,
                    VALID_DO_NOT_FLAG_LINE,
                    "- no id here",
                ],
            ),
            &[],
            "",
        );
        assert_one_problem_contains(&problems, "duplicate");
        assert_one_problem_contains(&problems, "- no id here");
    }

    #[test]
    fn check_rejects_known_non_issue_id_that_is_not_kebab_case() {
        let problems = problems_with_readme(
            &readme_with(&["Reth exit"]),
            &detect_with_do_not_flag(&[], &["- `Reth exit`: intended."]),
            &[],
            "",
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "kebab-case");
    }

    #[test]
    fn check_reports_duplicate_known_non_issue_once() {
        let problems = problems_with_readme(
            &readme_with(&["reth-exit", "reth-exit"]),
            &detect_with(&[]),
            &[],
            "",
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_one_problem_contains(&problems, "duplicate known non-issue reth-exit");
    }

    #[test]
    fn check_rejects_duplicate_instance_rows() {
        let later_date = VALID_INSTANCE.replace("2026-10-13", "2026-10-20");
        for second in [VALID_INSTANCE.to_owned(), later_date] {
            let problems = problems_for(
                &detect_with(&[VALID_LINE]),
                &[("uncapped-x", VALID_CLASS)],
                &format!("{VALID_INSTANCE}\n{second}"),
            );
            assert_eq!(problems.len(), 1, "{problems:?}");
            assert_one_problem_contains(&problems, "instance 2");
            assert_one_problem_contains(&problems, "same class, site and fix as instance 1");
        }
    }

    #[test]
    fn check_accepts_same_site_with_another_fix() {
        let other_fix = VALID_INSTANCE.replace(r##"fix = "#1574""##, r##"fix = "#1678""##);
        let problems = problems_for(
            &detect_with(&[VALID_LINE]),
            &[("uncapped-x", VALID_CLASS)],
            &format!("{VALID_INSTANCE}\n{other_fix}"),
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn check_reports_missing_readme() {
        let tmp = TempDirBuilder::new().build();
        write_catalogue(tmp.path(), "", &detect_with(&[]), &[], "");
        fs::remove_file(tmp.path().join(README_FILE)).unwrap();
        assert_one_problem_contains(&check(tmp.path()), README_FILE);
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
