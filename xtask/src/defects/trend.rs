//! Per-class instance counts from the instance log, so we know which classes to prioritise.

use crate::defects::catalogue::{Catalogue, Instance, Stream, parse_release};
use chrono::NaiveDate;
use std::cmp::Reverse;
use std::collections::BTreeSet;

/// Which instances count as recent.
#[derive(Debug, Clone)]
pub enum Window {
    /// Instances found on or after this date.
    Since(NaiveDate),
    /// Instances whose `released_in` tag is in this set (see [`last_releases`]).
    Releases(BTreeSet<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendRow {
    pub id: String,
    pub severity: String,
    pub total: usize,
    pub in_window: Option<usize>,
    pub last_found: Option<NaiveDate>,
    pub last_release: Option<String>,
}

type Version = (u64, u64, u64);

fn version(instance: &Instance) -> Option<Version> {
    parse_release(&instance.released_in).map(|(_, version)| version)
}

fn found(instance: &Instance) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(&instance.found, "%Y-%m-%d").ok()
}

/// The `count` highest release tags of each stream (testnet and mainnet apart), in version
/// order. `tags` is every tag in the repo: a release with no logged instance still counts, and
/// tags that are not release tags are ignored.
pub fn last_releases(tags: &[String], count: usize) -> BTreeSet<String> {
    let mut releases: Vec<(Stream, Version, &String)> = tags
        .iter()
        .filter_map(|tag| parse_release(tag).map(|(stream, version)| (stream, version, tag)))
        .collect();
    releases.sort_by(|a, b| b.1.cmp(&a.1));
    [Stream::Testnet, Stream::Mainnet]
        .into_iter()
        .flat_map(|stream| {
            releases
                .iter()
                .filter(move |(s, _, _)| *s == stream)
                .take(count)
                .map(|(_, _, tag)| (*tag).clone())
        })
        .collect()
}

/// One row per class, classes without instances included. Sorted by instances in the window
/// (descending), then total (descending), then id.
pub fn trend(catalogue: &Catalogue, window: Option<&Window>) -> Vec<TrendRow> {
    let is_recent = |instance: &Instance| match window {
        None => false,
        Some(Window::Since(since)) => found(instance).is_some_and(|d| d >= *since),
        Some(Window::Releases(tags)) => tags.contains(&instance.released_in),
    };

    let mut rows: Vec<TrendRow> = catalogue
        .classes
        .iter()
        .map(|class| {
            let instances: Vec<&Instance> = catalogue
                .instances
                .iter()
                .filter(|i| i.class == class.slug)
                .collect();
            TrendRow {
                id: class.slug.clone(),
                severity: class.meta.severity.to_string(),
                total: instances.len(),
                in_window: window.map(|_| instances.iter().filter(|i| is_recent(i)).count()),
                last_found: instances.iter().filter_map(|i| found(i)).max(),
                last_release: instances
                    .iter()
                    .filter_map(|i| version(i).map(|v| (v, &i.released_in)))
                    .max()
                    .map(|(_, tag)| tag.clone()),
            }
        })
        .collect();
    rows.sort_by_key(|r| (Reverse(r.in_window), Reverse(r.total), r.id.clone()));
    rows
}

/// Renders rows as an aligned plain-text table with a header line.
pub fn render_trend(rows: &[TrendRow], with_window: bool) -> String {
    let mut table: Vec<Vec<String>> = Vec::with_capacity(rows.len() + 1);
    let mut header = vec!["id", "severity", "total"];
    if with_window {
        header.push("window");
    }
    header.extend(["last-found", "last-release"]);
    table.push(header.into_iter().map(str::to_owned).collect());
    for row in rows {
        let mut cells = vec![row.id.clone(), row.severity.clone(), row.total.to_string()];
        if with_window {
            cells.push(row.in_window.unwrap_or(0).to_string());
        }
        cells.push(
            row.last_found
                .map_or_else(|| "-".to_owned(), |d| d.to_string()),
        );
        cells.push(row.last_release.clone().unwrap_or_else(|| "-".to_owned()));
        table.push(cells);
    }
    let widths: Vec<usize> = (0..table[0].len())
        .map(|col| table.iter().map(|r| r[col].len()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in &table {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defects::catalogue::{ClassFile, ClassMeta, FoundBy, Instance, Severity};

    fn class(id: &str, severity: Severity) -> ClassFile {
        ClassFile {
            slug: id.to_owned(),
            meta: ClassMeta {
                id: id.to_owned(),
                severity,
                scope: "always".to_owned(),
                flag: "always".to_owned(),
                paths: vec![],
                patterns: vec![],
            },
            body: String::new(),
        }
    }

    fn instance(class: &str, found: &str, released_in: &str) -> Instance {
        Instance {
            class: class.to_owned(),
            site: "x.rs::f".to_owned(),
            found: found.to_owned(),
            found_by: FoundBy::Review,
            fix: "#1".to_owned(),
            released_in: released_in.to_owned(),
            note: None,
        }
    }

    fn fixture() -> Catalogue {
        Catalogue {
            classes: vec![
                class("a", Severity::Dos),
                class("b", Severity::Fork),
                class("c", Severity::Stall),
            ],
            instances: vec![
                instance("a", "2026-01-10", "testnet-3.0.0"),
                instance("a", "2026-09-01", "testnet-4.1.0"),
                instance("a", "2026-10-20", "testnet-4.1.1"),
                instance("b", "2026-10-25", "mainnet-4.1.1"),
            ],
            ..Catalogue::default()
        }
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn ids(rows: &[TrendRow]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn trend_no_window() {
        let rows = trend(&fixture(), None);
        assert_eq!(ids(&rows), ["a", "b", "c"]);
        assert_eq!(rows[0].total, 3);
        assert_eq!(rows[0].in_window, None);
        assert_eq!(rows[0].last_found, Some(date("2026-10-20")));
        assert_eq!(rows[0].last_release.as_deref(), Some("testnet-4.1.1"));
        assert_eq!(rows[0].severity, "dos");
        assert_eq!(rows[2].total, 0);
        assert_eq!(rows[2].last_found, None);
        assert_eq!(rows[2].last_release, None);
    }

    #[test]
    fn trend_since() {
        let rows = trend(&fixture(), Some(&Window::Since(date("2026-10-01"))));
        assert_eq!(ids(&rows), ["a", "b", "c"]);
        assert_eq!(rows[0].in_window, Some(1));
        assert_eq!(rows[1].in_window, Some(1));
        assert_eq!(rows[2].in_window, Some(0));
    }

    #[test]
    fn trend_window_orders_before_total() {
        let rows = trend(&fixture(), Some(&Window::Since(date("2026-10-21"))));
        assert_eq!(ids(&rows), ["b", "a", "c"]);
    }

    fn tags(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn last_releases_takes_highest_versions_per_stream() {
        let all = tags(&[
            "testnet-4.1.0",
            "testnet-10.0.0",
            "testnet-4.1.1",
            "mainnet-4.0.9",
            "mainnet-4.0.5",
            "testnet-latest",
            "backup/x",
        ]);
        assert_eq!(
            last_releases(&all, 1),
            BTreeSet::from(["mainnet-4.0.9".to_owned(), "testnet-10.0.0".to_owned()])
        );
        assert_eq!(
            last_releases(&all, 2),
            BTreeSet::from([
                "mainnet-4.0.5".to_owned(),
                "mainnet-4.0.9".to_owned(),
                "testnet-10.0.0".to_owned(),
                "testnet-4.1.1".to_owned(),
            ])
        );
    }

    #[test]
    fn trend_releases_counts_rows_whose_tag_is_in_the_set() {
        // A release with no logged row still moves the window: testnet-4.2.0 has no row.
        let window = Window::Releases(last_releases(
            &tags(&["testnet-4.1.1", "testnet-4.2.0", "mainnet-4.1.1"]),
            1,
        ));
        let rows = trend(&fixture(), Some(&window));
        assert_eq!(ids(&rows), ["b", "a", "c"]);
        assert_eq!(rows[0].in_window, Some(1));
        assert_eq!(rows[1].in_window, Some(0));
    }

    #[test]
    fn trend_empty() {
        let rows = trend(&Catalogue::default(), None);
        assert!(rows.is_empty());
        let rendered = render_trend(&[], false);
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
    }

    #[test]
    fn render_includes_window_column_only_when_set() {
        let rows = trend(&fixture(), None);
        assert!(
            !render_trend(&rows, false)
                .lines()
                .next()
                .unwrap()
                .contains("window")
        );
        assert!(
            render_trend(&rows, true)
                .lines()
                .next()
                .unwrap()
                .contains("window")
        );
    }
}
