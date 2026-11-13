//! Model-free scan of a diff: match each class's patterns against the lines the diff adds. Hits
//! are leads for a reviewer to verify, not findings.

use crate::defects::catalogue::Catalogue;
use eyre::eyre;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedLine {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// Hunk header counts: `@@ -a[,b] +c[,d] @@`. A missing count means 1.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32)> {
    let mut ranges = line.strip_prefix("@@ ")?.split(' ');
    let range_count = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (_, old_count) = range_count(ranges.next()?.strip_prefix('-')?)?;
    let (new_start, new_count) = range_count(ranges.next()?.strip_prefix('+')?)?;
    Some((old_count, new_start, new_count))
}

/// The path of a `+++ ` header target, without its `b/` prefix. Git appends a tab to a path that
/// has a space, and quotes a path that has a `"`, a `\\` or a control character.
fn header_path(target: &str) -> Option<String> {
    let target = target.strip_suffix('\t').unwrap_or(target);
    let path = match target.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(quoted) => {
            let mut path = String::with_capacity(quoted.len());
            let mut chars = quoted.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    path.push(c);
                    continue;
                }
                match chars.next() {
                    Some('t') => path.push('\t'),
                    Some('n') => path.push('\n'),
                    Some(other) => path.push(other),
                    None => {}
                }
            }
            path
        }
        None => target.to_owned(),
    };
    path.strip_prefix("b/").map(str::to_owned)
}

/// Parses `git diff -U0 --find-renames` output into the lines it adds. Hunk line counts decide
/// which lines are content, so an added line that itself starts with `++` is not taken for a file
/// header. Deleted files (`+++ /dev/null`) and binary files add no lines.
pub fn parse_added_lines(diff: &str) -> Vec<AddedLine> {
    let mut added = Vec::new();
    let mut path: Option<String> = None;
    let mut old_left = 0_u32;
    let mut new_left = 0_u32;
    let mut next_line = 0_u32;
    for line in diff.lines() {
        if old_left > 0 || new_left > 0 {
            if let Some(text) = line.strip_prefix('+') {
                new_left = new_left.saturating_sub(1);
                if let Some(path) = &path {
                    added.push(AddedLine {
                        path: path.clone(),
                        line: next_line,
                        text: text.to_owned(),
                    });
                }
                next_line += 1;
                continue;
            }
            if line.starts_with('-') {
                old_left = old_left.saturating_sub(1);
                continue;
            }
            if line.starts_with('\\') {
                continue;
            }
            // Any other line ends the hunk early (malformed counts); parse it as a header.
            old_left = 0;
            new_left = 0;
        }
        if let Some(target) = line.strip_prefix("+++ ") {
            path = header_path(target);
        } else if line.starts_with("diff --git ") {
            path = None;
        } else if let Some((old_count, new_start, new_count)) = parse_hunk_header(line) {
            old_left = old_count;
            new_left = new_count;
            next_line = new_start;
        }
    }
    added
}

/// Every line of a file the diff does not show (an untracked file), as added lines.
pub fn file_as_added_lines(path: &str, text: &str) -> Vec<AddedLine> {
    (1..)
        .zip(text.lines())
        .map(|(line, text)| AddedLine {
            path: path.to_owned(),
            line,
            text: text.to_owned(),
        })
        .collect()
}

#[derive(Debug)]
pub struct CompiledClass {
    pub id: String,
    pub paths: Vec<glob::Pattern>,
    pub patterns: Vec<regex::Regex>,
}

/// Compiles the classes that declare patterns. Fails on any pattern or glob that does not compile,
/// so a scan never skips a class without notice.
pub fn compile(catalogue: &Catalogue) -> eyre::Result<Vec<CompiledClass>> {
    catalogue
        .classes
        .iter()
        .filter(|class| !class.meta.patterns.is_empty())
        .map(|class| {
            let id = &class.slug;
            let paths = class
                .meta
                .paths
                .iter()
                .map(|glob| {
                    glob::Pattern::new(glob)
                        .map_err(|err| eyre!("class {id}: invalid paths glob {glob:?}: {err}"))
                })
                .collect::<eyre::Result<_>>()?;
            let patterns = class
                .meta
                .patterns
                .iter()
                .map(|pattern| {
                    regex::Regex::new(pattern)
                        .map_err(|err| eyre!("class {id}: invalid regex {pattern:?}: {err}"))
                })
                .collect::<eyre::Result<_>>()?;
            Ok(CompiledClass {
                id: id.clone(),
                paths,
                patterns,
            })
        })
        .collect()
}

/// `*` and `?` do not match `/`; `**` still matches any number of directories.
const PATH_MATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanHit {
    pub class: String,
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// One hit per (class, added line) that matches. Sorted by path, line, class.
pub fn scan(classes: &[CompiledClass], added: &[AddedLine]) -> Vec<ScanHit> {
    let mut hits: Vec<ScanHit> = classes
        .iter()
        .flat_map(|class| {
            added
                .iter()
                .filter(|line| {
                    (class.paths.is_empty()
                        || class
                            .paths
                            .iter()
                            .any(|p| p.matches_with(&line.path, PATH_MATCH)))
                        && class.patterns.iter().any(|r| r.is_match(&line.text))
                })
                .map(|line| ScanHit {
                    class: class.id.clone(),
                    path: line.path.clone(),
                    line: line.line,
                    text: line.text.clone(),
                })
        })
        .collect();
    hits.sort_by(|a, b| (&a.path, a.line, &a.class).cmp(&(&b.path, b.line, &b.class)));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defects::catalogue::{ClassFile, ClassMeta, Severity};

    fn added(path: &str, line: u32, text: &str) -> AddedLine {
        AddedLine {
            path: path.to_owned(),
            line,
            text: text.to_owned(),
        }
    }

    fn catalogue_with(paths: &[&str], patterns: &[&str]) -> Catalogue {
        Catalogue {
            classes: vec![ClassFile {
                slug: "uncapped-x".to_owned(),
                meta: ClassMeta {
                    id: "uncapped-x".to_owned(),
                    severity: Severity::Dos,
                    scope: "always".to_owned(),
                    flag: "always".to_owned(),
                    paths: paths.iter().map(|p| (*p).to_owned()).collect(),
                    patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
                },
                body: String::new(),
            }],
            ..Catalogue::default()
        }
    }

    #[test]
    fn parses_simple_hunk() {
        let diff = "diff --git a/crates/p2p/src/a.rs b/crates/p2p/src/a.rs
index 1..2 100644
--- a/crates/p2p/src/a.rs
+++ b/crates/p2p/src/a.rs
@@ -10,0 +11,2 @@ fn f() {
+let a = 1;
+let b = 2;
";
        assert_eq!(
            parse_added_lines(diff),
            [
                added("crates/p2p/src/a.rs", 11, "let a = 1;"),
                added("crates/p2p/src/a.rs", 12, "let b = 2;"),
            ]
        );
    }

    #[test]
    fn parses_hunk_without_count() {
        let diff = "--- a/x.rs
+++ b/x.rs
@@ -3 +4 @@
-old
+new
";
        assert_eq!(parse_added_lines(diff), [added("x.rs", 4, "new")]);
    }

    #[test]
    fn rename_counts_under_new_path() {
        let diff = "diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1,0 +2 @@
+added
";
        assert_eq!(parse_added_lines(diff), [added("new.rs", 2, "added")]);
    }

    #[test]
    fn deleted_and_binary_yield_nothing() {
        let diff = "diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
diff --git a/x.png b/x.png
index 1..2 100644
Binary files a/x.png and b/x.png differ
";
        assert!(parse_added_lines(diff).is_empty());
    }

    #[test]
    fn multiple_files() {
        let diff = "--- a/a.rs
+++ b/a.rs
@@ -1,0 +5 @@
+in a
--- a/b.rs
+++ b/b.rs
@@ -7,0 +9 @@
+in b
";
        assert_eq!(
            parse_added_lines(diff),
            [added("a.rs", 5, "in a"), added("b.rs", 9, "in b")]
        );
    }

    #[test]
    fn added_line_that_starts_with_plus_signs_is_kept() {
        let diff = "--- a/a.rs
+++ b/a.rs
@@ -1,0 +1 @@
+++counter;
";
        assert_eq!(parse_added_lines(diff), [added("a.rs", 1, "++counter;")]);
    }

    #[test]
    fn scan_matches_regex_within_paths() {
        let classes = compile(&catalogue_with(&["crates/p2p/**"], &[r"\.json\(\)"])).unwrap();
        let lines = [
            added("crates/p2p/src/a.rs", 5, "let v = resp.json().await?;"),
            added("crates/types/src/b.rs", 7, "x.json()"),
        ];
        let hits = scan(&classes, &lines);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].class, "uncapped-x");
        assert_eq!(hits[0].path, "crates/p2p/src/a.rs");
        assert_eq!(hits[0].line, 5);
    }

    #[test]
    fn scan_empty_paths_matches_everywhere() {
        let classes = compile(&catalogue_with(&[], &[r"\.json\(\)"])).unwrap();
        let lines = [
            added("crates/p2p/src/a.rs", 5, "let v = resp.json().await?;"),
            added("crates/types/src/b.rs", 7, "x.json()"),
        ];
        assert_eq!(scan(&classes, &lines).len(), 2);
    }

    #[test]
    fn scan_one_hit_per_class_and_line() {
        let classes = compile(&catalogue_with(&[], &[r"\.json\(\)", r"resp\."])).unwrap();
        let lines = [added("a.rs", 5, "let v = resp.json().await?;")];
        assert_eq!(scan(&classes, &lines).len(), 1);
    }

    #[test]
    fn path_with_space_drops_trailing_tab() {
        let diff = "--- a/a b.rs\t
+++ b/a b.rs\t
@@ -1,0 +1 @@
+x
";
        assert_eq!(parse_added_lines(diff), [added("a b.rs", 1, "x")]);
    }

    #[test]
    fn quoted_path_is_unquoted() {
        let diff = r#"--- "a/q\"x\\y.rs"
+++ "b/q\"x\\y.rs"
@@ -1,0 +1 @@
+x
"#;
        assert_eq!(parse_added_lines(diff), [added(r#"q"x\y.rs"#, 1, "x")]);
    }

    #[test]
    fn untracked_file_lines_are_all_added() {
        assert_eq!(
            file_as_added_lines("n.rs", "a\nb\n"),
            [added("n.rs", 1, "a"), added("n.rs", 2, "b")]
        );
    }

    #[test]
    fn star_glob_does_not_cross_directories() {
        let classes = compile(&catalogue_with(&["crates/p2p/src/*.rs"], &["x"])).unwrap();
        let lines = [
            added("crates/p2p/src/a.rs", 1, "x"),
            added("crates/p2p/src/sub/b.rs", 1, "x"),
        ];
        let hits = scan(&classes, &lines);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].path, "crates/p2p/src/a.rs");
    }

    #[test]
    fn seed_hash_class_matches_turbofish_and_from() {
        let (catalogue, problems) = crate::defects::catalogue::load(&crate::defects::default_dir());
        assert!(problems.is_empty(), "{problems:?}");
        let classes = compile(&catalogue).unwrap();
        let path = "crates/actors/src/block_tree_service.rs";
        for text in [
            "let mut seen = HashSet::<H256>::new();",
            "let m = HashMap::<H256, Vec<u8>>::with_capacity(4);",
            "let m = HashMap::from([(a, b)]);",
            "let s = HashSet::from_iter(ids);",
        ] {
            let hits = scan(&classes, &[added(path, 1, text)]);
            assert!(
                hits.iter()
                    .any(|h| h.class == "hash-iteration-order-in-consensus"),
                "no hit for {text:?}"
            );
        }
    }

    #[test]
    fn scan_rejects_invalid_catalogue() {
        assert!(compile(&catalogue_with(&[], &["("])).is_err());
        assert!(compile(&catalogue_with(&["[",], &["x"])).is_err());
    }
}
