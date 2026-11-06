//! `mrm --import FILE [--dry-run]`: add series by title in bulk.
//!
//! Each non-empty, non-`#` line is `title` or `title<TAB>read_through`. A
//! title is searched on Asura, then MangaCK, then MangaDex (whichever are
//! enabled); only a result whose title matches exactly (ignoring case and
//! punctuation) is added — near misses are listed, never guessed. Chapters
//! numbered up to `read_through` are marked read. Series already in the
//! library are skipped. Adding marks chapters notified, so nothing is pushed.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::PgPool;

use crate::db;
use crate::scraper::{Registry, SearchResult};

/// Search order: the sources the user reads on most come first.
const SOURCE_ORDER: [&str; 3] = ["asura", "mangack", "mangadex"];

/// Pause between series so a bulk import does not hammer the sources.
const PAUSE: Duration = Duration::from_millis(1500);

#[derive(Debug, PartialEq)]
pub struct ImportLine {
    pub title:        String,
    pub read_through: Option<f64>,
}

/// Parse one line of an import file; None for blanks and `#` comments.
pub fn parse_line(line: &str) -> Option<ImportLine> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split('\t');
    let title = parts.next()?.trim().to_string();
    let read_through = parts.next()
        .and_then(|n| n.trim().parse::<f64>().ok())
        .filter(|n| *n > 0.0);
    (!title.is_empty()).then_some(ImportLine { title, read_through })
}

/// Lowercase letters and digits only: "Myst, Might, Mayhem" == "myst might mayhem".
pub fn normalize(title: &str) -> String {
    title.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The result whose title matches `wanted` exactly after `normalize`.
pub fn exact_match<'a>(wanted: &str, results: &'a [SearchResult]) -> Option<&'a SearchResult> {
    let key = normalize(wanted);
    results.iter().find(|r| normalize(&r.title) == key)
}

pub async fn run(pool: &PgPool, registry: &Registry, path: &str, dry_run: bool) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let lines: Vec<ImportLine> = text.lines().filter_map(parse_line).collect();
    let mut have: HashSet<String> = db::fetch_all_manhwa(pool).await?
        .iter().map(|m| normalize(&m.title)).collect();
    let sources: Vec<&str> = SOURCE_ORDER.iter().copied()
        .filter(|s| registry.contains_key(s)).collect();

    let (mut added, mut skipped, mut missing, mut failed) = (0, 0, Vec::new(), Vec::new());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(PAUSE).await;
        }
        if have.contains(&normalize(&line.title)) {
            println!("= {} (already in library)", line.title);
            skipped += 1;
            continue;
        }
        let mut found = None;
        let mut near: Vec<String> = Vec::new();
        for source in &sources {
            match registry[source].search(&line.title).await {
                Ok(results) => {
                    if let Some(r) = exact_match(&line.title, &results) {
                        found = Some(r.clone());
                        break;
                    }
                    near.extend(results.iter().take(3).map(|r| format!("{} [{}]", r.title, r.source)));
                }
                Err(e) => near.push(format!("{source} search failed: {e:#}")),
            }
        }
        let Some(hit) = found else {
            println!("? {} — no exact match{}", line.title,
                     if near.is_empty() { String::new() } else { format!("; saw: {}", near.join(" | ")) });
            missing.push(line.title.clone());
            continue;
        };
        if dry_run {
            println!("+ {} → {} [{}]{}", line.title, hit.source_url, hit.source,
                     line.read_through.map(|n| format!(", read through {n}")).unwrap_or_default());
            continue;
        }
        match add_one(pool, registry, &hit, line.read_through).await {
            Ok(summary) => {
                println!("+ {} [{}] {summary}", line.title, hit.source);
                have.insert(normalize(&line.title));
                added += 1;
            }
            Err(e) => {
                println!("! {} [{}]: {e:#}", line.title, hit.source);
                failed.push(line.title.clone());
            }
        }
    }

    println!("\n{added} added, {skipped} already there, {} not found, {} failed{}",
             missing.len(), failed.len(), if dry_run { " (dry run: nothing written)" } else { "" });
    for t in missing.iter().chain(&failed) {
        println!("  - {t}");
    }
    Ok(())
}

async fn add_one(
    pool: &PgPool,
    registry: &Registry,
    hit: &SearchResult,
    read_through: Option<f64>,
) -> Result<String> {
    let series = registry[hit.source.as_str()].get_series(&hit.source_url).await?;
    let id = db::insert_manhwa_with_chapters(pool, &series, &hit.source).await?;
    let read = match read_through {
        Some(n) => db::mark_read_through(pool, id, n).await?,
        None => 0,
    };
    let status = db::recompute_status(pool, id).await?;
    Ok(format!("{} chapters, {read} marked read, {}", series.chapters.len(), status.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(title: &str) -> SearchResult {
        SearchResult {
            title: title.into(), cover_url: None, source_url: format!("https://x/{title}"),
            pub_status: "ongoing".into(), source: "asura".into(),
        }
    }

    #[test]
    fn parses_title_and_read_through() {
        assert_eq!(parse_line("Nano Machine\t337"),
                   Some(ImportLine { title: "Nano Machine".into(), read_through: Some(337.0) }));
        assert_eq!(parse_line("  Hellogin  "),
                   Some(ImportLine { title: "Hellogin".into(), read_through: None }));
        assert_eq!(parse_line("Tales\t534.1").unwrap().read_through, Some(534.1));
        assert_eq!(parse_line("X\t0").unwrap().read_through, None);
        assert_eq!(parse_line("# comment"), None);
        assert_eq!(parse_line("   "), None);
    }

    #[test]
    fn matches_ignore_case_and_punctuation_only() {
        let rs = [result("Myst, Might, Mayhem"), result("The Demon God"), result("Infinite Gacha")];
        assert_eq!(exact_match("myst might mayhem", &rs).unwrap().title, "Myst, Might, Mayhem");
        assert!(exact_match("Demon God", &rs).is_none(), "no partial matches");
        assert!(exact_match("Pick Me Up, Infinite Gacha", &rs).is_none());
        assert_eq!(normalize("Becoming the Cheon Clan’s Mad Dog"), normalize("Becoming the Cheon Clan's Mad Dog"));
    }
}
