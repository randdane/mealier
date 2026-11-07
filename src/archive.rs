//! `mealier import-archive`: recipes out of the crawler's archive.db (spec, "Site archive and `import-archive`").
use std::io::Write;
use std::path::Path;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Row};

use crate::{Db, Error, Importer, Microdata, Page, Result, Scope};

#[derive(Debug, Default)]
pub struct Report {
    pub imported: usize,
    pub dup: usize,
    pub skipped: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub image_rejected: Vec<(String, String)>,
}

/// As the crawler normalizes: scheme and host lowercased, default port, query and fragment dropped, trailing slash removed (not from a bare "/").
pub fn normalize(u: &str) -> String {
    match url::Url::parse(u.trim()) {
        Ok(mut p) => {
            p.set_query(None);
            p.set_fragment(None);
            let s = p.as_str();
            if p.path() == "/" { s.to_string() } else { s.trim_end_matches('/').to_string() }
        }
        Err(_) => u.trim().trim_end_matches('/').to_string(),
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "imported {}, dup {}, skipped {}, image rejected {}, failed {}", self.imported, self.dup, self.skipped.len(), self.image_rejected.len(), self.failed.len())?;
        for (kind, rows) in [("skipped", &self.skipped), ("image rejected", &self.image_rejected), ("failed", &self.failed)] {
            for (url, why) in rows {
                write!(f, "\n{kind}\t{url}\t{why}")?;
            }
        }
        Ok(())
    }
}

/// A recipe page: its archive key (`pages.url`, what the ok/dup lines print), where it ended up (the recipe source and dedupe key), and the mapped recipe.
struct Candidate {
    url: String,
    final_url: String,
    input: crate::RecipeInput,
}

/// `<group-slug>/<household-slug>` to the scope of that household's first enabled admin.
async fn resolve_scope(db: &Db, household: &str) -> Result<Scope> {
    let (gs, hs) = household.split_once('/').ok_or_else(|| Error::Validation("--household must be <group-slug>/<household-slug>".into()))?;
    let row = sqlx::query("SELECT h.id, h.group_id FROM households h JOIN groups g ON g.id = h.group_id WHERE g.slug = ? AND h.slug = ?")
        .bind(gs)
        .bind(hs)
        .fetch_optional(db.pool())
        .await?
        .ok_or(Error::NotFound("household"))?;
    let (household_id, group_id): (i64, i64) = (row.get(0), row.get(1));
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE household_id = ? AND role = 'admin' AND disabled = 0 ORDER BY id LIMIT 1")
        .bind(household_id)
        .fetch_optional(db.pool())
        .await?
        .ok_or_else(|| Error::Validation(format!("household {household} has no enabled admin")))?;
    Ok(Scope { user_id, group_id, household_id, admin: true })
}

/// Every 200 page that maps to a recipe, with keywords lowercased. Pages that do not map go to `skipped`, undecodable ones to `failed`.
async fn load_candidates(conn: &mut sqlx::SqliteConnection, sc: &Scope, db: &Db, report: &mut Report) -> Result<Vec<Candidate>> {
    let (units, foods) = (db.units(sc).await?, db.foods(sc).await?);
    let pages = sqlx::query("SELECT final_url, json_ld, raw_html, url FROM pages WHERE status_code = 200 ORDER BY url").fetch_all(&mut *conn).await?;
    let mut found = Vec::new();
    for p in pages {
        let (final_url, ld, raw, url): (String, Option<String>, Option<Vec<u8>>, String) = (p.get(0), p.get(1), p.get(2), p.get(3));
        let ld: Option<serde_json::Value> = match (ld, raw) {
            (Some(ld), _) => serde_json::from_str(&ld).ok(),
            (None, Some(raw)) => {
                let parsed = async {
                    let html = String::from_utf8_lossy(&zstd::decode_all(&raw[..])?).into_owned();
                    Microdata.import(&Page { url: final_url.clone(), final_url: final_url.clone(), html }).await
                };
                match parsed.await {
                    Ok(ld) => ld,
                    Err(e) => {
                        report.failed.push((url, e.to_string()));
                        continue;
                    }
                }
            }
            (None, None) => None,
        };
        match ld.and_then(|ld| crate::map_jsonld(&ld, &final_url, &units, &foods)) {
            Some((mut input, _image)) => {
                input.tags = input.tags.iter().map(|t| t.to_lowercase()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                found.push(Candidate { url, final_url, input });
            }
            None => report.skipped.push((url, "no recipe found".into())),
        }
    }
    Ok(found)
}

/// Keywords are counted once, over every recipe page, before anything is imported; rarer ones are dropped.
fn keep_common_tags(found: &mut [Candidate], min_tag_count: u32) {
    let mut counts = std::collections::HashMap::<String, u32>::new();
    for c in found.iter() {
        for t in &c.input.tags {
            *counts.entry(t.clone()).or_default() += 1;
        }
    }
    for c in found {
        c.input.tags.retain(|t| counts[t] >= min_tag_count);
    }
}

/// Creates each candidate not already in the group, printing `ok`/`dup` lines keyed by the archive url.
async fn write_recipes(conn: &mut sqlx::SqliteConnection, db: &Db, sc: &Scope, found: Vec<Candidate>, images_dir: &Path, out: &mut impl Write, report: &mut Report) -> Result<()> {
    let mut have: std::collections::HashSet<String> =
        sqlx::query_scalar::<_, String>("SELECT source_url FROM recipes WHERE group_id = ? AND source_url IS NOT NULL").bind(sc.group_id).fetch_all(db.pool()).await?.iter().map(|u| normalize(u)).collect();
    for Candidate { url, final_url, input } in found {
        if !have.insert(normalize(&final_url)) {
            writeln!(out, "dup\t{url}")?;
            report.dup += 1;
            continue;
        }
        let bytes: Option<Vec<u8>> = sqlx::query_scalar("SELECT bytes FROM images WHERE page_url = ? AND status = 'ok'").bind(&url).fetch_optional(&mut *conn).await?.flatten();
        let photo = match bytes {
            Some(b) => crate::images::prepare(&b).await.map_err(|e| report.image_rejected.push((url.clone(), e.to_string()))).ok(),
            None => None,
        };
        if let Err(e) = db.create_recipe_with(sc, input, photo.as_ref().map(|p| (p, images_dir))).await {
            report.failed.push((url, e.to_string()));
            continue;
        }
        writeln!(out, "ok\t{url}")?;
        report.imported += 1;
    }
    Ok(())
}

pub async fn import_archive(db: &Db, archive: &Path, household: &str, min_tag_count: u32, images_dir: &Path, out: &mut impl Write) -> Result<Report> {
    let sc = resolve_scope(db, household).await?;
    let mut conn = SqliteConnectOptions::new().filename(archive).read_only(true).immutable(true).connect().await?;
    let mut report = Report::default();
    let mut found = load_candidates(&mut conn, &sc, db, &mut report).await?;
    keep_common_tags(&mut found, min_tag_count);
    write_recipes(&mut conn, db, &sc, found, images_dir, out, &mut report).await?;
    Ok(report)
}
