//! One-time importer for Mealie v3.28 backup zips: everything in one
//! transaction into an empty DB, or nothing.
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use zip::ZipArchive;

use crate::units::KNOWN_UNITS;
use crate::recipes::{IngredientInput, Nutrition, RecipeInput, insert_recipe};
use crate::{Db, Dimension, Error, Result};

/// `alembic_version` of Mealie v3.28.0, the only schema this importer reads.
const MEALIE_SCHEMA: &str = "3527efeeec34";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub units: usize,
    pub foods: usize,
    pub recipes: usize,
    pub tags: usize,
    pub categories: usize,
    pub cookbooks: usize,
    pub tools: usize,
    pub batches: usize,
    pub comments: usize,
    pub meal_plan_entries: usize,
    pub shopping_lists: usize,
    pub shopping_list_items: usize,
    pub users: usize,
    pub images: usize,
    /// Records the importer left out, one line each.
    pub skipped: Vec<String>,
}

type MRow = Map<String, Value>;
type Tx<'a> = Transaction<'a, Sqlite>;

struct Backup(Map<String, Value>);

impl Backup {
    fn rows(&self, table: &str) -> Vec<&MRow> {
        self.0.get(table).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_object).collect()).unwrap_or_default()
    }
}

fn s<'a>(r: &'a MRow, k: &str) -> Option<&'a str> {
    r.get(k).and_then(Value::as_str)
}
fn f(r: &MRow, k: &str) -> Option<f64> {
    r.get(k).and_then(Value::as_f64)
}
fn flag(r: &MRow, k: &str) -> bool {
    r.get(k).and_then(Value::as_bool).unwrap_or(false)
}
fn id<'a>(r: &'a MRow, k: &str) -> Result<&'a str> {
    s(r, k).ok_or_else(|| Error::Validation(format!("backup row without {k}")))
}

async fn remember(tx: &mut Tx<'_>, kind: &str, uuid: &str, local: i64) -> Result<()> {
    sqlx::query("INSERT OR REPLACE INTO import_ids (kind, mealie_uuid, local_id) VALUES (?, ?, ?)")
        .bind(kind)
        .bind(uuid)
        .bind(local)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn local(tx: &mut Tx<'_>, kind: &str, uuid: &str) -> Result<i64> {
    sqlx::query_scalar("SELECT local_id FROM import_ids WHERE kind = ? AND mealie_uuid = ?")
        .bind(kind)
        .bind(uuid)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::Validation(format!("backup refers to a {kind} it does not contain")))
}

async fn local_opt(tx: &mut Tx<'_>, kind: &str, uuid: Option<&str>) -> Result<Option<i64>> {
    match uuid {
        Some(u) => Ok(Some(local(tx, kind, u).await?)),
        None => Ok(None),
    }
}

/// Alias rows grouped by their parent's id, as a JSON array.
fn aliases(b: &Backup, table: &str, parent: &str) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for r in b.rows(table) {
        if let (Some(p), Some(n)) = (s(r, parent), s(r, "name")) {
            m.entry(p.to_string()).or_default().push(n.to_string());
        }
    }
    m
}

fn classify(u: &MRow) -> (Dimension, Option<f64>) {
    for k in ["name", "abbreviation", "plural_name", "plural_abbreviation"] {
        let Some(v) = s(u, k).map(str::to_lowercase) else { continue };
        if let Some((_, d, factor)) = KNOWN_UNITS.iter().find(|(names, ..)| names.contains(&v.as_str())) {
            return (*d, Some(*factor));
        }
    }
    (Dimension::Other, None)
}

fn archive_err(e: impl std::fmt::Display) -> Error {
    Error::Validation(format!("not a readable Mealie backup: {e}"))
}

fn read_backup(zip: &Path) -> Result<(Backup, ZipArchive<File>)> {
    let mut archive = ZipArchive::new(File::open(zip)?).map_err(archive_err)?;
    let mut json = String::new();
    archive.by_name("database.json").map_err(archive_err)?.read_to_string(&mut json).map_err(archive_err)?;
    let Value::Object(db) = serde_json::from_str(&json).map_err(archive_err)? else {
        return Err(archive_err("database.json is not an object"));
    };
    let b = Backup(db);
    let version = b.rows("alembic_version").first().and_then(|r| s(r, "version_num"));
    if version != Some(MEALIE_SCHEMA) {
        return Err(Error::Validation(format!(
            "only Mealie v3.28 backups (schema {MEALIE_SCHEMA}) can be imported, this one is {}",
            version.unwrap_or("unknown")
        )));
    }
    Ok((b, archive))
}

async fn require_empty(tx: &mut Tx<'_>) -> Result<()> {
    for t in ["recipes", "units", "foods", "users", "tags", "categories", "cookbooks", "tools", "batches", "shopping_lists", "meal_plan_entries", "images"] {
        let n: i64 = sqlx::query(&format!("SELECT count(*) FROM {t}")).fetch_one(&mut **tx).await?.get(0);
        if n > 0 {
            return Err(Error::Validation(format!("the database is not empty ({t} has rows); import into a fresh one")));
        }
    }
    Ok(())
}

pub async fn import_mealie_backup(db: &Db, zip: &Path, images_dir: &Path) -> Result<ImportReport> {
    let (b, mut archive) = read_backup(zip)?;
    let mut report = ImportReport::default();
    let mut tx = db.pool.begin().await?;
    require_empty(&mut tx).await?;

    import_units_and_foods(&mut tx, &b, &mut report).await?;
    import_users(&mut tx, &b, &mut report).await?;
    import_recipes(&mut tx, &b, &mut report).await?;
    import_history(&mut tx, &b, &mut report).await?;
    import_meal_plans(&mut tx, &b, &mut report).await?;
    import_shopping(&mut tx, &b, &mut report).await?;
    let files = import_images(&mut tx, &b, &mut archive, &mut report).await?;

    // Files go down last. Content-addressed and harmless if the commit fails:
    // the GC step collects unreferenced ones.
    std::fs::create_dir_all(images_dir)?;
    for (name, bytes) in files {
        std::fs::write(images_dir.join(name), bytes)?;
    }
    tx.commit().await?;
    Ok(report)
}

async fn import_units_and_foods(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    let unit_aliases = aliases(b, "ingredient_units_aliases", "unit_id");
    for u in b.rows("ingredient_units") {
        let (uuid, name) = (id(u, "id")?, id(u, "name")?);
        let (dim, factor) = classify(u);
        let al = serde_json::to_string(unit_aliases.get(uuid).unwrap_or(&vec![]))?;
        let r = sqlx::query("INSERT OR IGNORE INTO units (name, aliases, dimension, factor) VALUES (?, ?, ?, ?)")
            .bind(name)
            .bind(al)
            .bind(dim.as_str())
            .bind(factor)
            .execute(&mut **tx)
            .await?;
        if r.rows_affected() == 0 {
            report.skipped.push(format!("unit {name:?} appears twice; the second was merged into the first"));
        } else {
            report.units += 1;
        }
        let lid = sqlx::query_scalar("SELECT id FROM units WHERE name = ?").bind(name).fetch_one(&mut **tx).await?;
        remember(tx, "unit", uuid, lid).await?;
    }
    let food_aliases = aliases(b, "ingredient_foods_aliases", "food_id");
    for fo in b.rows("ingredient_foods") {
        let (uuid, name) = (id(fo, "id")?, id(fo, "name")?);
        let al = serde_json::to_string(food_aliases.get(uuid).unwrap_or(&vec![]))?;
        let r = sqlx::query("INSERT OR IGNORE INTO foods (name, aliases) VALUES (?, ?)").bind(name).bind(al).execute(&mut **tx).await?;
        if r.rows_affected() == 0 {
            report.skipped.push(format!("food {name:?} appears twice; the second was merged into the first"));
        } else {
            report.foods += 1;
        }
        let lid = sqlx::query_scalar("SELECT id FROM foods WHERE name = ?").bind(name).fetch_one(&mut **tx).await?;
        remember(tx, "food", uuid, lid).await?;
    }
    let labels = b.rows("multi_purpose_labels").len();
    if labels > 0 {
        report.skipped.push(format!("{labels} labels (food and shopping-list categories) are not imported"));
    }
    Ok(())
}

/// Inserts every row's name into `table` and remembers its id. Returns id -> name.
async fn vocabulary(tx: &mut Tx<'_>, b: &Backup, src: &str, table: &str, kind: &str) -> Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    for r in b.rows(src) {
        let (uuid, name) = (id(r, "id")?, id(r, "name")?);
        let lid = sqlx::query(&format!("INSERT OR IGNORE INTO {table} (name) VALUES (?)")).bind(name).execute(&mut **tx).await?.last_insert_rowid();
        remember(tx, kind, uuid, lid).await?;
        names.insert(uuid.to_string(), name.to_string());
    }
    Ok(names)
}

/// Mealie stores nutrition as text such as "583" or "12 g".
fn number(r: &MRow, k: &str) -> Option<f64> {
    let t = s(r, k)?.trim();
    t[..t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len())].parse().ok()
}

fn positioned<'a>(b: &'a Backup, table: &str, recipe: &str) -> Vec<&'a MRow> {
    let mut rows: Vec<_> = b.rows(table).into_iter().filter(|r| s(r, "recipe_id") == Some(recipe)).collect();
    rows.sort_by_key(|r| r.get("position").and_then(Value::as_i64).unwrap_or(0));
    rows
}

fn linked(b: &Backup, table: &str, key: &str, recipe: &str, names: &HashMap<String, String>) -> Vec<String> {
    b.rows(table)
        .into_iter()
        .filter(|r| s(r, "recipe_id") == Some(recipe))
        .filter_map(|r| s(r, key).and_then(|k| names.get(k)).cloned())
        .collect()
}

async fn import_recipes(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    let tags = vocabulary(tx, b, "tags", "tags", "tag").await?;
    let categories = vocabulary(tx, b, "categories", "categories", "category").await?;
    let cookbooks = vocabulary(tx, b, "cookbooks", "cookbooks", "cookbook").await?;
    let tools = vocabulary(tx, b, "tools", "tools", "tool").await?;
    (report.tags, report.categories, report.cookbooks, report.tools) = (tags.len(), categories.len(), cookbooks.len(), tools.len());
    if !cookbooks.is_empty() {
        report.skipped.push(format!("{} cookbooks are imported by name only: Mealie defines their recipes by a filter", cookbooks.len()));
    }

    let mut dropped_nutrition = false;
    let mut dropped_titles = 0;
    for r in b.rows("recipes") {
        let uuid = id(r, "id")?;
        let mut ingredients = Vec::new();
        for i in positioned(b, "recipes_ingredients", uuid) {
            let note = s(i, "note").unwrap_or("").to_string();
            let q = f(i, "quantity").filter(|q| *q > 0.0);
            if s(i, "title").is_some_and(|t| !t.is_empty()) {
                dropped_titles += 1;
            }
            ingredients.push(IngredientInput {
                qty_min: q,
                qty_max: q,
                unit_id: local_opt(tx, "unit", s(i, "unit_id")).await?,
                food_id: local_opt(tx, "food", s(i, "food_id")).await?,
                original_text: s(i, "original_text").map_or_else(|| note.clone(), str::to_string),
                note,
            });
        }
        let nutrition = b.rows("recipe_nutrition").into_iter().find(|n| s(n, "recipe_id") == Some(uuid)).and_then(|n| {
            dropped_nutrition |= ["fiber", "sodium", "sugar", "cholesterol", "saturated_fat", "trans_fat", "unsaturated_fat"]
                .iter()
                .any(|k| number(n, &format!("{k}_content")).is_some());
            let v = Nutrition {
                calories: number(n, "calories"),
                protein_g: number(n, "protein_content"),
                fat_g: number(n, "fat_content"),
                carbs_g: number(n, "carbohydrate_content"),
            };
            (v != Nutrition::default()).then_some(v)
        });
        let input = RecipeInput {
            name: id(r, "name")?.to_string(),
            description: s(r, "description").unwrap_or("").to_string(),
            source_url: s(r, "org_url").filter(|u| !u.is_empty()).map(str::to_string),
            nutrition,
            tags: linked(b, "recipes_to_tags", "tag_id", uuid, &tags),
            categories: linked(b, "recipes_to_categories", "category_id", uuid, &categories),
            cookbooks: vec![],
            tools: linked(b, "recipes_to_tools", "tool_id", uuid, &tools),
            ingredients,
            steps: positioned(b, "recipe_instructions", uuid).into_iter().filter_map(|i| s(i, "text").map(str::to_string)).collect(),
        };
        let recipe = insert_recipe(tx, input).await?;
        remember(tx, "recipe", uuid, recipe.id).await?;
        report.recipes += 1;
    }
    if dropped_nutrition {
        report.skipped.push("nutrition other than calories, protein, fat and carbohydrate is not imported".into());
    }
    if dropped_titles > 0 {
        report.skipped.push(format!("{dropped_titles} ingredient section titles are not imported"));
    }
    Ok(())
}

/// No passwords or OIDC links come across: the first OIDC login links by email.
async fn import_users(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    for u in b.rows("users") {
        let uuid = id(u, "id")?;
        let lid = sqlx::query("INSERT INTO users (username, email, full_name, role, disabled) VALUES (?, ?, ?, ?, ?)")
            .bind(id(u, "username")?)
            .bind(id(u, "email")?)
            .bind(s(u, "full_name").unwrap_or(""))
            .bind(if flag(u, "admin") { "admin" } else { "user" })
            .bind(s(u, "locked_at").is_some())
            .execute(&mut **tx)
            .await?
            .last_insert_rowid();
        remember(tx, "user", uuid, lid).await?;
        report.users += 1;
    }
    Ok(())
}

fn date(timestamp: &str) -> &str {
    timestamp.get(..10).unwrap_or(timestamp)
}

/// Timeline "made it" comments become done batches; Mealie's own comments
/// are kept as comments; everything else is listed as skipped.
async fn import_history(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    let mut system = 0;
    let mut made_dates: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in b.rows("recipe_timeline_events") {
        let (recipe, subject) = (id(e, "recipe_id")?, s(e, "subject").unwrap_or(""));
        let ts = s(e, "timestamp").unwrap_or("");
        let lower = subject.to_lowercase();
        match s(e, "event_type") {
            Some("system") => system += 1,
            Some("comment") if lower.contains("made this") || lower.contains("made it") => {
                sqlx::query("INSERT INTO batches (status, recipe_id, source, made_on, notes) VALUES ('done', ?, 'imported', ?, ?)")
                    .bind(local(tx, "recipe", recipe).await?)
                    .bind(date(ts))
                    .bind(s(e, "message").unwrap_or(""))
                    .execute(&mut **tx)
                    .await?;
                made_dates.entry(recipe).or_default().push(date(ts));
                report.batches += 1;
            }
            kind => report.skipped.push(format!(
                "timeline event {subject:?} ({}) on {ts}: not a made-it event",
                kind.unwrap_or("unknown type")
            )),
        }
    }
    if system > 0 {
        report.skipped.push(format!("{system} system timeline events are not imported"));
    }
    for h in b.rows("households_to_recipes") {
        let (recipe, last) = (id(h, "recipe_id")?, s(h, "last_made"));
        if let Some(last) = last
            && !made_dates.get(recipe).is_some_and(|d| d.contains(&date(last)))
        {
            report.skipped.push(format!("recipe {recipe} last_made {last} has no made-it event, so no batch was created"));
        }
    }
    for c in b.rows("recipe_comments") {
        sqlx::query("INSERT INTO recipe_comments (recipe_id, user_id, text, created_at) VALUES (?, ?, ?, ?)")
            .bind(local(tx, "recipe", id(c, "recipe_id")?).await?)
            .bind(local_opt(tx, "user", s(c, "user_id")).await?)
            .bind(s(c, "text").unwrap_or(""))
            .bind(s(c, "created_at").unwrap_or(""))
            .execute(&mut **tx)
            .await?;
        report.comments += 1;
    }
    Ok(())
}

async fn import_meal_plans(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    for m in b.rows("group_meal_plans") {
        sqlx::query("INSERT INTO meal_plan_entries (date, slot, recipe_id, title, text) VALUES (?, ?, ?, ?, ?)")
            .bind(id(m, "date")?)
            .bind(s(m, "entry_type").unwrap_or("dinner"))
            .bind(local_opt(tx, "recipe", s(m, "recipe_id")).await?)
            .bind(s(m, "title").unwrap_or(""))
            .bind(s(m, "text").unwrap_or(""))
            .execute(&mut **tx)
            .await?;
        report.meal_plan_entries += 1;
    }
    Ok(())
}

async fn import_shopping(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    for l in b.rows("shopping_lists") {
        let uuid = id(l, "id")?;
        let lid = sqlx::query("INSERT INTO shopping_lists (name) VALUES (?)").bind(s(l, "name").unwrap_or("")).execute(&mut **tx).await?.last_insert_rowid();
        remember(tx, "shopping_list", uuid, lid).await?;
        report.shopping_lists += 1;
    }
    let item_recipes: HashMap<&str, &str> = b
        .rows("shopping_list_item_recipe_reference")
        .into_iter()
        .filter_map(|r| Some((s(r, "shopping_list_item_id")?, s(r, "recipe_id")?)))
        .collect();
    for i in b.rows("shopping_list_items") {
        let q = f(i, "quantity").filter(|q| *q > 0.0);
        sqlx::query("INSERT INTO shopping_list_items (list_id, qty, unit_id, food_id, note, checked, recipe_id) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(local(tx, "shopping_list", id(i, "shopping_list_id")?).await?)
            .bind(q)
            .bind(local_opt(tx, "unit", s(i, "unit_id")).await?)
            .bind(local_opt(tx, "food", s(i, "food_id")).await?)
            .bind(s(i, "note").unwrap_or(""))
            .bind(flag(i, "checked"))
            .bind(local_opt(tx, "recipe", item_recipes.get(id(i, "id")?).copied()).await?)
            .execute(&mut **tx)
            .await?;
        report.shopping_list_items += 1;
    }
    if !b.rows("shopping_list_recipe_reference").is_empty() {
        report.skipped.push("shopping lists' own recipe references are not imported; items keep theirs".into());
    }
    Ok(())
}

/// Images are stored as `<images_dir>/<image_hash>.<ext>`.
pub(crate) fn image_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn dashed(uuid: &str) -> String {
    format!("{}-{}-{}-{}-{}", &uuid[..8], &uuid[8..12], &uuid[12..16], &uuid[16..20], &uuid[20..])
}

/// Each recipe's `original.<ext>`; Mealie's thumbnails are regenerated later.
/// Returns (file name, bytes) to write once everything else has gone in.
async fn import_images(
    tx: &mut Tx<'_>,
    b: &Backup,
    archive: &mut ZipArchive<File>,
    report: &mut ImportReport,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    for r in b.rows("recipes") {
        let uuid = id(r, "id")?;
        if uuid.len() != 32 {
            continue;
        }
        let prefix = format!("data/recipes/{}/images/original.", dashed(uuid));
        let Some(name) = archive.file_names().filter_map(|n| n.ok()).find(|n| n.starts_with(&prefix)).map(|n| n.to_string()) else { continue };
        let ext = name[prefix.len()..].to_lowercase();
        let mime = match ext.as_str() {
            "webp" => "image/webp",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            _ => {
                report.skipped.push(format!("image {name}: unknown type"));
                continue;
            }
        };
        let mut bytes = Vec::new();
        archive.by_name(&name).map_err(archive_err)?.read_to_end(&mut bytes).map_err(archive_err)?;
        let hash = image_hash(&bytes);
        if files.insert(format!("{hash}.{ext}"), bytes).is_none() {
            sqlx::query("INSERT INTO images (hash, mime) VALUES (?, ?)").bind(&hash).bind(mime).execute(&mut **tx).await?;
            report.images += 1;
        }
        sqlx::query("INSERT INTO recipe_images (recipe_id, hash) VALUES (?, ?)")
            .bind(local(tx, "recipe", uuid).await?)
            .bind(&hash)
            .execute(&mut **tx)
            .await?;
    }
    Ok(files.into_iter().collect())
}
