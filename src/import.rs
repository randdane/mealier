//! One-time importer for Mealie v3.28 backup zips: everything in one
//! transaction into an empty DB, or nothing.
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value};
use sqlx::{Row, Sqlite, Transaction};
use zip::ZipArchive;

use crate::images::Processed;
use crate::filter::{self, By, Filter, SetField};
use crate::tenancy::{free_slug, slugify};
use crate::units::KNOWN_UNITS;
use crate::recipes::{IngredientInput, LineDetail, Note, Nutrition, RecipeInput, Settings, StepDetail, Substitution, check_extras, insert_recipe};
use crate::{Db, Dimension, Error, Result};

/// `alembic_version` of Mealie v3.28.0, the only schema this importer reads.
const MEALIE_SCHEMA: &str = "3527efeeec34";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub groups: usize,
    pub households: usize,
    pub units: usize,
    pub foods: usize,
    pub labels: usize,
    pub recipes: usize,
    pub tags: usize,
    pub categories: usize,
    pub cookbooks: usize,
    pub meal_plan_rules: usize,
    pub tools: usize,
    pub batches: usize,
    pub comments: usize,
    pub meal_plan_entries: usize,
    pub shopping_lists: usize,
    pub shopping_list_items: usize,
    pub users: usize,
    pub images: usize,
    /// Recipe attachments stored.
    pub assets: usize,
    /// Rows of favourites (users' `is_favorite`), and of ratings kept (half stars).
    pub favourites: usize,
    pub ratings: usize,
    pub recipe_actions: usize,
    pub foods_on_hand: usize,
    pub tools_owned: usize,
    pub share_links: usize,
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

/// The database must hold nothing but, at most, the local admin and the empty "Home" tenant made for
/// them. Returns Home's group id, if it is there: the import replaces it.
async fn require_empty(tx: &mut Tx<'_>) -> Result<Option<i64>> {
    let not_empty = |t: &str| Error::Validation(format!("the database is not empty ({t} has rows); import into a fresh one"));
    for t in ["recipes", "units", "foods", "tags", "categories", "cookbooks", "tools", "batches", "shopping_lists", "meal_plan_entries", "images"] {
        let n: i64 = sqlx::query(&format!("SELECT count(*) FROM {t}")).fetch_one(&mut **tx).await?.get(0);
        if n > 0 {
            return Err(not_empty(t));
        }
    }
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE local_admin = 0").fetch_one(&mut **tx).await?;
    if users > 0 {
        return Err(not_empty("users"));
    }
    let groups: Vec<(i64, String)> = sqlx::query_as("SELECT id, slug FROM groups").fetch_all(&mut **tx).await?;
    let households: Vec<(String, i64)> = sqlx::query_as("SELECT slug, group_id FROM households").fetch_all(&mut **tx).await?;
    let only_home = matches!((groups.as_slice(), households.as_slice()), ([(g, gs)], [(hs, hg)]) if gs == "home" && hs == "home" && hg == g);
    if !(groups.is_empty() && households.is_empty()) && !only_home {
        return Err(not_empty("groups"));
    }
    Ok(sqlx::query_scalar("SELECT id FROM groups").fetch_optional(&mut **tx).await?)
}

pub async fn import_mealie_backup(db: &Db, zip: &Path, images_dir: &Path) -> Result<ImportReport> {
    let (b, mut archive) = read_backup(zip)?;
    let mut report = ImportReport::default();
    let images = prepare_images(&b, &mut archive, &mut report).await?;
    let assets = prepare_assets(&b, &mut archive, &mut report)?;
    let mut tx = db.pool.begin().await?;
    let home = require_empty(&mut tx).await?;
    if let Some(g) = home {
        // Out of the way of an imported group called "home"; deleted below once the local admin has moved.
        sqlx::query("UPDATE groups SET slug = '~home' WHERE id = ?").bind(g).execute(&mut *tx).await?;
    }

    import_tenants(&mut tx, &b, &mut report).await?;
    import_units_and_foods(&mut tx, &b, &mut report).await?;
    import_users(&mut tx, &b, &mut report).await?;
    import_recipes(&mut tx, &b, &images, &assets, &mut report).await?;
    import_history(&mut tx, &b, &mut report).await?;
    import_meal_plans(&mut tx, &b, &mut report).await?;
    import_cookbooks(&mut tx, &b, &mut report).await?;
    import_rules(&mut tx, &b, &mut report).await?;
    import_shopping(&mut tx, &b, &mut report).await?;
    import_household_features(&mut tx, &b, &mut report).await?;
    let files = import_images(&mut tx, &images, &mut report).await?;
    if let Some(g) = home {
        retire_home(&mut tx, g).await?;
    }

    // Files go down last. Content-addressed and harmless if the commit fails:
    // the GC step collects unreferenced ones.
    for photo in files {
        photo.write(images_dir)?;
    }
    for a in assets.values().flatten() {
        a.write(&images_dir.join("assets"))?;
    }
    tx.commit().await?;
    Ok(report)
}

/// Mealie's `{first_day_of_week, private_*, ...}` row as the columns mealier reads plus the rest as JSON.
fn extra(r: &MRow, used: &[&str]) -> Result<String> {
    let rest: Map<String, Value> = r.iter().filter(|(k, _)| !["id", "group_id", "household_id", "created_at", "update_at"].contains(&k.as_str()) && !used.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    Ok(serde_json::to_string(&rest)?)
}

/// Every group and household in the backup, with their preferences. The first household becomes the default one.
async fn import_tenants(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    if b.rows("households").is_empty() {
        return Err(Error::Validation("the backup contains no households".into()));
    }
    for g in b.rows("groups") {
        let uuid = id(g, "id")?;
        let name = id(g, "name")?;
        let made = crate::tenancy::insert_group(tx, name, s(g, "slug").unwrap_or(name)).await?;
        if let Some(p) = b.rows("group_preferences").into_iter().find(|p| s(p, "group_id") == Some(uuid)) {
            sqlx::query("UPDATE group_preferences SET private_group = ?, extra = ? WHERE group_id = ?").bind(flag(p, "private_group")).bind(extra(p, &["private_group"])?).bind(made.id).execute(&mut **tx).await?;
        }
        remember(tx, "group", uuid, made.id).await?;
        report.groups += 1;
    }
    let mut first = None;
    for h in b.rows("households") {
        let uuid = id(h, "id")?;
        let name = id(h, "name")?;
        let group = local(tx, "group", id(h, "group_id")?).await?;
        let made = crate::tenancy::insert_household(tx, group, name, s(h, "slug").unwrap_or(name)).await?;
        if let Some(p) = b.rows("household_preferences").into_iter().find(|p| s(p, "household_id") == Some(uuid)) {
            sqlx::query("UPDATE household_preferences SET first_day_of_week = ?, private_household = ?, extra = ? WHERE household_id = ?")
                .bind(p.get("first_day_of_week").and_then(Value::as_i64).unwrap_or(0))
                .bind(flag(p, "private_household"))
                .bind(extra(p, &["first_day_of_week", "private_household"])?)
                .bind(made.id)
                .execute(&mut **tx)
                .await?;
        }
        remember(tx, "household", uuid, made.id).await?;
        first.get_or_insert(made.id);
        report.households += 1;
    }
    crate::tenancy::set_default(tx, first.expect("households is not empty")).await?;
    Ok(())
}

/// The empty "Home" made for the local admin gives way to the imported tenants: the admin moves
/// into the default household, then Home is deleted.
async fn retire_home(tx: &mut Tx<'_>, group: i64) -> Result<()> {
    let default: String = sqlx::query_scalar("SELECT value FROM instance_settings WHERE key = 'default_household_id'").fetch_one(&mut **tx).await?;
    let default: i64 = default.parse().map_err(|_| Error::Validation("bad default household".into()))?;
    sqlx::query("UPDATE users SET household_id = ? WHERE household_id IN (SELECT id FROM households WHERE group_id = ?)").bind(default).bind(group).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM household_preferences WHERE household_id IN (SELECT id FROM households WHERE group_id = ?)").bind(group).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM households WHERE group_id = ?").bind(group).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM group_preferences WHERE group_id = ?").bind(group).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM groups WHERE id = ?").bind(group).execute(&mut **tx).await?;
    Ok(())
}

/// A row of `table` must be in `group`: a backup that mixes tenants is refused, not repaired.
async fn same_group(tx: &mut Tx<'_>, table: &str, row: i64, group: i64, what: &str) -> Result<()> {
    let ok: Option<i64> = sqlx::query_scalar(&format!("SELECT 1 FROM {table} WHERE id = ? AND group_id = ?")).bind(row).bind(group).fetch_optional(&mut **tx).await?;
    ok.map(|_| ()).ok_or_else(|| Error::Validation(format!("backup {what} belongs to another group")))
}

/// The household a Mealie record belongs to: its user's, when that user is in `group`; otherwise the group's first.
async fn household_for(tx: &mut Tx<'_>, user_uuid: Option<&str>, group: i64) -> Result<i64> {
    if let Some(u) = user_uuid
        && let Some(uid) = sqlx::query_scalar::<_, i64>("SELECT local_id FROM import_ids WHERE kind = 'user' AND mealie_uuid = ?").bind(u).fetch_optional(&mut **tx).await?
    {
        let h: Option<i64> = sqlx::query_scalar("SELECT u.household_id FROM users u JOIN households h ON h.id = u.household_id WHERE u.id = ? AND h.group_id = ?").bind(uid).bind(group).fetch_optional(&mut **tx).await?;
        if let Some(h) = h {
            return Ok(h);
        }
    }
    let first: Option<i64> = sqlx::query_scalar("SELECT min(id) FROM households WHERE group_id = ?").bind(group).fetch_one(&mut **tx).await?;
    first.ok_or_else(|| Error::Validation("backup group has no household".into()))
}

async fn import_units_and_foods(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    let unit_aliases = aliases(b, "ingredient_units_aliases", "unit_id");
    for u in b.rows("ingredient_units") {
        let (uuid, name) = (id(u, "id")?, id(u, "name")?);
        let group = local(tx, "group", id(u, "group_id")?).await?;
        let (dim, factor) = classify(u);
        let al = serde_json::to_string(unit_aliases.get(uuid).unwrap_or(&vec![]))?;
        let r = sqlx::query("INSERT OR IGNORE INTO units (group_id, name, aliases, dimension, factor) VALUES (?, ?, ?, ?, ?)")
            .bind(group)
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
        let lid = sqlx::query_scalar("SELECT id FROM units WHERE group_id = ? AND name = ?").bind(group).bind(name).fetch_one(&mut **tx).await?;
        remember(tx, "unit", uuid, lid).await?;
    }
    for l in b.rows("multi_purpose_labels") {
        let (uuid, name) = (id(l, "id")?, id(l, "name")?);
        let group = local(tx, "group", id(l, "group_id")?).await?;
        let color = s(l, "color").map(str::to_lowercase).filter(|c| c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|d| d.is_ascii_hexdigit()));
        if color.is_none() {
            report.skipped.push(format!("label {name:?} has colour {:?}, not #rrggbb; it got Mealie's default grey", s(l, "color").unwrap_or_default()));
        }
        let r = sqlx::query("INSERT OR IGNORE INTO labels (group_id, name, color) VALUES (?, ?, ?)").bind(group).bind(name).bind(color.unwrap_or_else(|| "#959595".into())).execute(&mut **tx).await?;
        if r.rows_affected() == 0 {
            report.skipped.push(format!("label {name:?} appears twice; the second was merged into the first"));
        } else {
            report.labels += 1;
        }
        let lid = sqlx::query_scalar("SELECT id FROM labels WHERE group_id = ? AND name = ?").bind(group).bind(name).fetch_one(&mut **tx).await?;
        remember(tx, "label", uuid, lid).await?;
    }
    let food_aliases = aliases(b, "ingredient_foods_aliases", "food_id");
    for fo in b.rows("ingredient_foods") {
        let (uuid, name) = (id(fo, "id")?, id(fo, "name")?);
        let group = local(tx, "group", id(fo, "group_id")?).await?;
        let al = serde_json::to_string(food_aliases.get(uuid).unwrap_or(&vec![]))?;
        let label = local_opt(tx, "label", s(fo, "label_id")).await?;
        if let Some(l) = label {
            same_group(tx, "labels", l, group, "food's label").await?;
        }
        let r = sqlx::query("INSERT OR IGNORE INTO foods (group_id, name, aliases, label_id) VALUES (?, ?, ?, ?)").bind(group).bind(name).bind(al).bind(label).execute(&mut **tx).await?;
        if r.rows_affected() == 0 {
            report.skipped.push(format!("food {name:?} appears twice; the second was merged into the first"));
        } else {
            report.foods += 1;
        }
        let lid = sqlx::query_scalar("SELECT id FROM foods WHERE group_id = ? AND name = ?").bind(group).bind(name).fetch_one(&mut **tx).await?;
        remember(tx, "food", uuid, lid).await?;
    }
    import_food_features(tx, b, report).await
}

/// Mealie's `<owner>_extras` rows (`key_name`, `value`) as the JSON object kept in the owner's `extras` column, by owner id.
/// The owner column's name is in `owner_keys`: the fixture has these tables empty, so the names are Mealie's model names, unconfirmed.
fn extras_by_owner(b: &Backup, table: &str, owner_keys: &[&str]) -> HashMap<String, std::collections::BTreeMap<String, String>> {
    let mut out: HashMap<String, std::collections::BTreeMap<String, String>> = HashMap::new();
    for r in b.rows(table) {
        let (Some(o), Some(k)) = (owner(r, owner_keys), s(r, "key_name")) else { continue };
        let v = match r.get("value") {
            Some(Value::String(v)) => v.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        out.entry(o.to_string()).or_default().insert(k.to_string(), v);
    }
    out
}

/// Food extras and substitutions (`ingredient_food_extras`, `ingredient_foods_substitutions`).
async fn import_food_features(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    for (uuid, extras) in extras_by_owner(b, "ingredient_food_extras", &["ingredient_food_id", "food_id"]) {
        let food = local(tx, "food", &uuid).await?;
        sqlx::query("UPDATE foods SET extras = ? WHERE id = ?").bind(serde_json::to_string(&check_extras(extras)?)?).bind(food).execute(&mut **tx).await?;
    }
    for r in b.rows("ingredient_foods_substitutions") {
        let (food, sub) = (local(tx, "food", id(r, "food_id")?).await?, local(tx, "food", id(r, "substitute_food_id")?).await?);
        let groups: Vec<i64> = sqlx::query_scalar("SELECT group_id FROM foods WHERE id IN (?, ?)").bind(food).bind(sub).fetch_all(&mut **tx).await?;
        if groups.len() == 2 && groups[0] != groups[1] {
            return Err(Error::Validation("backup food substitution joins foods of two groups".into()));
        }
        if food == sub {
            report.skipped.push("a food substitution names the food itself; it was not imported".into());
            continue;
        }
        sqlx::query("INSERT OR IGNORE INTO food_substitutions (food_id, substitute_food_id, note) VALUES (?, ?, ?)").bind(food).bind(sub).bind(s(r, "note").unwrap_or("").trim()).execute(&mut **tx).await?;
    }
    Ok(())
}

/// Inserts every row's name into `table` and remembers its id. Returns id -> name.
async fn vocabulary(tx: &mut Tx<'_>, b: &Backup, src: &str, table: &str, kind: &str) -> Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    for r in b.rows(src) {
        let (uuid, name) = (id(r, "id")?, id(r, "name")?);
        let group = local(tx, "group", id(r, "group_id")?).await?;
        sqlx::query(&format!("INSERT OR IGNORE INTO {table} (group_id, name) VALUES (?, ?)")).bind(group).bind(name).execute(&mut **tx).await?;
        let lid = sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE group_id = ? AND name = ?")).bind(group).bind(name).fetch_one(&mut **tx).await?;
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

/// A recipe attachment read out of the zip, ready to store.
struct PreparedAsset {
    hash: String,
    bytes: Vec<u8>,
    name: String,
    icon: String,
    file_name: String,
}

impl PreparedAsset {
    /// Content-addressed, so writing it twice is harmless. Whole or not at all, like images.
    fn write(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{}.tmp", self.hash));
        std::fs::write(&tmp, &self.bytes)?;
        std::fs::rename(tmp, dir.join(&self.hash))?;
        Ok(())
    }
}

/// Each recipe's attachments (`recipe_assets` rows, files under `data/recipes/<id>/assets/`), keyed by the
/// recipe's Mealie id. Columns `name`, `icon`, `file_name` come from Mealie's `RecipeAsset` schema (the fixture's table is empty: unconfirmed).
/// A file that is missing, empty or over the 20 MB limit is reported and left out.
fn prepare_assets(b: &Backup, archive: &mut ZipArchive<File>, report: &mut ImportReport) -> Result<HashMap<String, Vec<PreparedAsset>>> {
    use sha2::{Digest, Sha256};
    let mut out: HashMap<String, Vec<PreparedAsset>> = HashMap::new();
    for r in b.rows("recipe_assets") {
        let (recipe, file_name) = (id(r, "recipe_id")?, s(r, "file_name").unwrap_or_default());
        let label = format!("asset {file_name:?} ({:?}) of recipe {recipe}", s(r, "name").unwrap_or_default());
        if recipe.len() != 32 || file_name.is_empty() {
            report.skipped.push(format!("{label}: no usable file name; not imported"));
            continue;
        }
        let mut bytes = Vec::new();
        match archive.by_name(&format!("data/recipes/{}/assets/{file_name}", dashed(recipe))) {
            Ok(mut f) => {
                // Bounded read: a lying size in the header cannot make this allocate more than the limit.
                if std::io::Read::take(&mut f, crate::MAX_ASSET_BYTES as u64 + 1).read_to_end(&mut bytes).is_err() {
                    report.skipped.push(format!("{label}: the file cannot be read; not imported"));
                    continue;
                }
            }
            Err(_) => {
                report.skipped.push(format!("{label}: the file is not in the backup; not imported"));
                continue;
            }
        }
        if bytes.is_empty() || bytes.len() > crate::MAX_ASSET_BYTES {
            report.skipped.push(format!("{label}: the file is {}; not imported", if bytes.is_empty() { "empty" } else { "over 20 MB" }));
            continue;
        }
        let hash = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        let file_name = crate::clean_file_name(file_name);
        let name = s(r, "name").map(str::trim).filter(|n| !n.is_empty()).map_or_else(|| file_name.clone(), |n| n.chars().take(200).collect());
        let list = out.entry(recipe.to_string()).or_default();
        if !list.iter().any(|a| a.hash == hash) {
            list.push(PreparedAsset { hash, bytes, name, icon: s(r, "icon").unwrap_or_default().chars().take(100).collect(), file_name });
        }
    }
    report.assets = out.values().map(Vec::len).sum();
    Ok(out)
}

/// Mealie's recipe settings as mealier's: `show_nutrition` is the opposite of `hide_nutrition`.
fn settings_of(b: &Backup, recipe: &str) -> Settings {
    let Some(r) = b.rows("recipe_settings").into_iter().find(|r| s(r, "recipe_id") == Some(recipe)) else { return Settings::default() };
    Settings {
        locked: flag(r, "locked"),
        hide_nutrition: r.get("show_nutrition").and_then(Value::as_bool).is_some_and(|show| !show),
        disable_comments: flag(r, "disable_comments"),
        public: flag(r, "public"),
        show_assets: flag(r, "show_assets"),
        landscape_view: flag(r, "landscape_view"),
        disable_amount: flag(r, "disable_amount"),
    }
}

/// True when a link or substitution row's owner column (any of `keys`) names this ingredient row, by its integer `id` or its `reference_id`.
fn names_ingredient(r: &MRow, keys: &[&str], ingredient: &MRow) -> bool {
    keys.iter().filter_map(|k| r.get(*k)).any(|v| {
        let text = match v {
            Value::String(t) => t.clone(),
            Value::Number(n) => n.to_string(),
            _ => return false,
        };
        ingredient.get("id").is_some_and(|i| match i {
            Value::String(t) => *t == text,
            Value::Number(n) => n.to_string() == text,
            _ => false,
        }) || s(ingredient, "reference_id") == Some(text.as_str())
    })
}

async fn import_recipes(tx: &mut Tx<'_>, b: &Backup, images: &HashMap<String, Processed>, assets: &HashMap<String, Vec<PreparedAsset>>, report: &mut ImportReport) -> Result<()> {
    let tags = vocabulary(tx, b, "tags", "tags", "tag").await?;
    let categories = vocabulary(tx, b, "categories", "categories", "category").await?;
    let tools = vocabulary(tx, b, "tools", "tools", "tool").await?;
    (report.tags, report.categories, report.tools) = (tags.len(), categories.len(), tools.len());
    let recipe_extras = extras_by_owner(b, "api_extras", &["recipee_id", "recipe_id"]);

    let mut dropped_nutrition = false;
    let mut dropped_summaries = 0;
    for r in b.rows("recipes") {
        let uuid = id(r, "id")?;
        let mut ingredients = Vec::new();
        let mut details = Vec::new();
        for i in positioned(b, "recipes_ingredients", uuid) {
            let note = s(i, "note").unwrap_or("").to_string();
            let q = f(i, "quantity").filter(|q| *q > 0.0);
            ingredients.push(IngredientInput {
                qty_min: q,
                qty_max: q,
                unit_id: local_opt(tx, "unit", s(i, "unit_id")).await?,
                food_id: local_opt(tx, "food", s(i, "food_id")).await?,
                original_text: s(i, "original_text").map_or_else(|| note.clone(), str::to_string),
                note,
            });
            // Mealie's reference id is what its instructions link to; it becomes the line's id. Substitutions name
            // the ingredient by its integer `id` or its `reference_id` (column names unconfirmed: the fixture's table is empty).
            let mut substitutions = Vec::new();
            for sub in b.rows("recipes_ingredients_substitutions").into_iter().filter(|x| names_ingredient(x, &["ingredient_id", "recipe_ingredient_id", "recipes_ingredients_id", "reference_id"], i)) {
                let food = match s(sub, "substitute_food_id") {
                    Some(f) => Some(sqlx::query_scalar::<_, String>("SELECT name FROM foods WHERE id = ?").bind(local(tx, "food", f).await?).fetch_one(&mut **tx).await?),
                    None => None,
                };
                substitutions.push(Substitution { qty: None, unit: None, food, note: s(sub, "note").unwrap_or("").to_string() });
            }
            details.push(LineDetail { uid: s(i, "reference_id").filter(|u| !u.is_empty()).map(str::to_string), section: s(i, "title").unwrap_or("").to_string(), substitutions });
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
        // Steps: the title, and the ingredient lines each links to (`recipe_ingredient_ref_link`: instruction_id, reference_id).
        let (mut steps, mut step_details) = (Vec::new(), Vec::new());
        for step in positioned(b, "recipe_instructions", uuid) {
            let Some(text) = s(step, "text") else { continue };
            if s(step, "summary").is_some_and(|x| !x.is_empty()) {
                dropped_summaries += 1;
            }
            let mut links: Vec<String> = Vec::new();
            for l in b.rows("recipe_ingredient_ref_link").into_iter().filter(|l| s(l, "instruction_id") == s(step, "id")) {
                let Some(reference) = s(l, "reference_id") else { continue };
                if !details.iter().any(|d: &LineDetail| d.uid.as_deref() == Some(reference)) {
                    report.skipped.push(format!("a step of recipe {uuid} links to the ingredient line {reference}, which the recipe does not have; the link was dropped"));
                } else if !links.iter().any(|x| x == reference) {
                    links.push(reference.to_string());
                }
            }
            steps.push(text.to_string());
            step_details.push(StepDetail { title: s(step, "title").unwrap_or("").to_string(), ingredient_uids: links });
        }
        let notes = b.rows("notes").into_iter().filter(|n| s(n, "recipe_id") == Some(uuid)).map(|n| Note { title: s(n, "title").unwrap_or("").to_string(), text: s(n, "text").unwrap_or("").to_string() }).collect();
        let own_assets = assets.get(uuid).map(Vec::as_slice).unwrap_or_default();
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
            ingredient_details: details,
            steps,
            step_details,
            notes: Some(notes),
            extras: Some(recipe_extras.get(uuid).cloned().unwrap_or_default()),
            settings: Some(settings_of(b, uuid)),
        };
        let group = local(tx, "group", id(r, "group_id")?).await?;
        let recipe = insert_recipe(tx, group, input, images.get(uuid).map(|p| p.hash.clone()).into_iter().collect(), own_assets.iter().map(|a| a.hash.clone()).collect()).await?;
        for a in own_assets {
            sqlx::query("INSERT OR IGNORE INTO assets (hash, size) VALUES (?, ?)").bind(&a.hash).bind(a.bytes.len() as i64).execute(&mut **tx).await?;
            sqlx::query("INSERT INTO recipe_assets (recipe_id, hash, name, icon, file_name) VALUES (?, ?, ?, ?, ?)").bind(recipe.id).bind(&a.hash).bind(&a.name).bind(&a.icon).bind(&a.file_name).execute(&mut **tx).await?;
        }
        remember(tx, "recipe", uuid, recipe.id).await?;
        report.recipes += 1;
    }
    if dropped_nutrition {
        report.skipped.push("nutrition other than calories, protein, fat and carbohydrate is not imported".into());
    }
    if dropped_summaries > 0 {
        report.skipped.push(format!("{dropped_summaries} step summaries are not imported (mealier steps have a title and text)"));
    }
    let note_links = b.rows("recipe_note_ref_link").len();
    if note_links > 0 {
        report.skipped.push(format!("{note_links} note references on steps are not imported; the notes themselves are"));
    }
    Ok(())
}

/// No passwords or OIDC links come across: the first OIDC login links by email.
async fn import_users(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    for u in b.rows("users") {
        let uuid = id(u, "id")?;
        let (household, group) = (local(tx, "household", id(u, "household_id")?).await?, local(tx, "group", id(u, "group_id")?).await?);
        same_group(tx, "households", household, group, "user's household").await?;
        // The local admin was here first. A backup user called "admin" is the one the owner knows; the local admin steps aside.
        sqlx::query("UPDATE users SET username = 'local-admin' WHERE local_admin = 1 AND username = ?").bind(id(u, "username")?).execute(&mut **tx).await?;
        let lid = sqlx::query("INSERT INTO users (username, email, full_name, role, disabled, household_id) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(id(u, "username")?)
            .bind(id(u, "email")?)
            .bind(s(u, "full_name").unwrap_or(""))
            .bind(if flag(u, "admin") { "admin" } else { "user" })
            .bind(s(u, "locked_at").is_some())
            .bind(household)
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
                let recipe_id = local(tx, "recipe", recipe).await?;
                let group: i64 = sqlx::query_scalar("SELECT group_id FROM recipes WHERE id = ?").bind(recipe_id).fetch_one(&mut **tx).await?;
                let household = household_for(tx, s(e, "user_id"), group).await?;
                sqlx::query("INSERT INTO batches (household_id, status, recipe_id, source, made_on, notes) VALUES (?, 'done', ?, 'imported', ?, ?)")
                    .bind(household)
                    .bind(recipe_id)
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
        let group = local(tx, "group", id(m, "group_id")?).await?;
        let household = household_for(tx, s(m, "user_id"), group).await?;
        let recipe = local_opt(tx, "recipe", s(m, "recipe_id")).await?;
        if let Some(r) = recipe {
            same_group(tx, "recipes", r, group, "meal plan entry's recipe").await?;
        }
        sqlx::query("INSERT INTO meal_plan_entries (household_id, date, slot, recipe_id, title, text) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(household)
            .bind(id(m, "date")?)
            .bind(s(m, "entry_type").unwrap_or("dinner"))
            .bind(recipe)
            .bind(s(m, "title").unwrap_or(""))
            .bind(s(m, "text").unwrap_or(""))
            .execute(&mut **tx)
            .await?;
        report.meal_plan_entries += 1;
    }
    Ok(())
}

/// The first of `keys` the row has: Mealie's link tables name their owner column differently from version to version.
fn owner<'a>(r: &'a MRow, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| s(r, k))
}

/// `tags.id IN ["a", "b"]` for the ids a legacy link table lists for `owner_id`; `None` when it lists none.
fn link_clause(b: &Backup, table: &str, owner_keys: &[&str], owner_id: &str, child: &str, field: &str, all: bool) -> Option<String> {
    let ids: Vec<String> = b.rows(table).into_iter().filter(|r| owner(r, owner_keys) == Some(owner_id)).filter_map(|r| s(r, child)).map(|c| format!("\"{c}\"")).collect();
    (!ids.is_empty()).then(|| format!("{field} {} [{}]", if all { "CONTAINS ALL" } else { "IN" }, ids.join(", ")))
}

/// The filter as written, with legacy link clauses ANDed on; the parsed form of that, with Mealie's uuids
/// turned into mealier ids; or, when it cannot be read, the string as written and `None` (with a report line).
async fn read_stored_filter(tx: &mut Tx<'_>, written: &str, clauses: Vec<String>, group: i64, what: &str, report: &mut ImportReport) -> Result<(String, Option<String>)> {
    let Err(e) = filter::parse(written) else {
        let query = match (written.trim().is_empty(), clauses.is_empty()) {
            (_, true) => written.to_string(),
            (true, false) => clauses.join(" AND "),
            (false, false) => format!("({written}) AND {}", clauses.join(" AND ")),
        };
        let mut parsed = filter::parse(&query).map_err(Error::from)?;
        if let Some(f) = &mut parsed {
            map_ids(tx, f, group, what, report).await?;
        }
        return Ok((query, parsed.as_ref().map(serde_json::to_string).transpose()?));
    };
    report.skipped.push(format!("{what}: {e}; kept as written, and it selects nothing"));
    Ok((written.to_string(), None))
}

/// Replaces each Mealie uuid in an `.id` list with the mealier id it became. A uuid the backup does not
/// hold becomes 0, which no row has (so it matches nothing, as in Mealie) and is reported; one that
/// belongs to another group refuses the whole import.
async fn map_ids(tx: &mut Tx<'_>, f: &mut Filter, group: i64, what: &str, report: &mut ImportReport) -> Result<()> {
    let mut slots: Vec<(SetField, &mut String)> = Vec::new();
    collect_ids(f, &mut slots);
    for (field, value) in slots {
        let (kind, table, noun) = match field {
            SetField::Tags => ("tag", "tags", "tag"),
            SetField::Categories => ("category", "categories", "category"),
            SetField::Tools => ("tool", "tools", "tool"),
        };
        let uuid = value.replace('-', "").to_lowercase();
        match sqlx::query_scalar::<_, i64>("SELECT local_id FROM import_ids WHERE kind = ? AND mealie_uuid = ?").bind(kind).bind(&uuid).fetch_optional(&mut **tx).await? {
            Some(local) => {
                same_group(tx, table, local, group, &format!("{what}'s {noun}")).await?;
                *value = local.to_string();
            }
            None => {
                report.skipped.push(format!("{what}: {noun} id {value} is not in the backup; it matches nothing"));
                *value = "0".into();
            }
        }
    }
    Ok(())
}

fn collect_ids<'a>(f: &'a mut Filter, out: &mut Vec<(SetField, &'a mut String)>) {
    match f {
        Filter::And(p) | Filter::Or(p) => p.iter_mut().for_each(|x| collect_ids(x, out)),
        Filter::Set { field, by: By::Id, values, .. } => out.extend(values.iter_mut().map(|v| (*field, v))),
        _ => {}
    }
}

/// Cookbooks with their filters. Mealie 3 keeps the filter in `query_filter_string`; the older
/// `cookbooks_to_*` link tables, if a backup still has rows in them, are ANDed on.
async fn import_cookbooks(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    for c in b.rows("cookbooks") {
        let (uuid, name) = (id(c, "id")?, id(c, "name")?);
        let group = local(tx, "group", id(c, "group_id")?).await?;
        let taken: Option<i64> = sqlx::query_scalar("SELECT id FROM cookbooks WHERE group_id = ? AND name = ?").bind(group).bind(name).fetch_optional(&mut **tx).await?;
        if taken.is_some() {
            report.skipped.push(format!("cookbook {name:?} appears twice; the second was not imported"));
            continue;
        }
        let clauses: Vec<String> = [
            link_clause(b, "cookbooks_to_tags", &["cookbook_id"], uuid, "tag_id", "tags.id", flag(c, "require_all_tags")),
            link_clause(b, "cookbooks_to_categories", &["cookbook_id"], uuid, "category_id", "recipe_category.id", flag(c, "require_all_categories")),
            link_clause(b, "cookbooks_to_tools", &["cookbook_id"], uuid, "tool_id", "tools.id", flag(c, "require_all_tools")),
        ]
        .into_iter()
        .flatten()
        .collect();
        let (query, json) = read_stored_filter(tx, s(c, "query_filter_string").unwrap_or(""), clauses, group, &format!("cookbook {name:?}"), report).await?;
        let slug = free_slug(tx, "cookbooks", Some(group), &slugify(s(c, "slug").filter(|x| !x.is_empty()).unwrap_or(name), "cookbook")).await?;
        let lid = sqlx::query("INSERT INTO cookbooks (group_id, name, description, slug, position, public, query_filter, filter_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(group)
            .bind(name)
            .bind(s(c, "description").unwrap_or(""))
            .bind(slug)
            .bind(c.get("position").and_then(Value::as_i64).unwrap_or(1))
            .bind(flag(c, "public"))
            .bind(query)
            .bind(json)
            .execute(&mut **tx)
            .await?
            .last_insert_rowid();
        remember(tx, "cookbook", uuid, lid).await?;
        report.cookbooks += 1;
    }
    Ok(())
}

/// Meal-plan rules: day and meal, the filter (plus legacy tag and category links), and the households
/// the rule is limited to. A day or meal mealier has no word for is reported and the rule left out.
async fn import_rules(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    const RULE_KEYS: [&str; 3] = ["group_plan_rule_id", "plan_rule_id", "rule_id"];
    for r in b.rows("group_meal_plan_rules") {
        let uuid = id(r, "id")?;
        let group = local(tx, "group", id(r, "group_id")?).await?;
        let (day, slot) = (s(r, "day").unwrap_or("unset"), s(r, "entry_type").unwrap_or("unset"));
        let label = format!("meal-plan rule ({day} {slot})");
        if day != "unset" && !crate::DAYS.contains(&day) || slot != "unset" && !crate::SLOTS.contains(&slot) {
            report.skipped.push(format!("{label}: mealier has no such day or meal; the rule was not imported"));
            continue;
        }
        let clauses: Vec<String> = [
            link_clause(b, "plan_rules_to_tags", &RULE_KEYS, uuid, "tag_id", "tags.id", false),
            link_clause(b, "plan_rules_to_categories", &RULE_KEYS, uuid, "category_id", "recipe_category.id", false),
        ]
        .into_iter()
        .flatten()
        .collect();
        let (query, json) = read_stored_filter(tx, s(r, "query_filter_string").unwrap_or(""), clauses, group, &label, report).await?;
        let lid = sqlx::query("INSERT INTO meal_plan_rules (group_id, day, slot, query_filter, filter_json) VALUES (?, ?, ?, ?, ?)")
            .bind(group)
            .bind(day)
            .bind(slot)
            .bind(query)
            .bind(json)
            .execute(&mut **tx)
            .await?
            .last_insert_rowid();
        for h in b.rows("plan_rules_to_households").into_iter().filter(|h| owner(h, &RULE_KEYS) == Some(uuid)) {
            let household = local(tx, "household", id(h, "household_id")?).await?;
            same_group(tx, "households", household, group, "meal-plan rule's household").await?;
            sqlx::query("INSERT OR IGNORE INTO meal_plan_rule_households (rule_id, household_id) VALUES (?, ?)").bind(lid).bind(household).execute(&mut **tx).await?;
        }
        remember(tx, "meal_plan_rule", uuid, lid).await?;
        report.meal_plan_rules += 1;
    }
    Ok(())
}

async fn import_shopping(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    let mut list_group: HashMap<&str, i64> = HashMap::new();
    for l in b.rows("shopping_lists") {
        let uuid = id(l, "id")?;
        let group = local(tx, "group", id(l, "group_id")?).await?;
        let household = household_for(tx, s(l, "user_id"), group).await?;
        let lid = sqlx::query("INSERT INTO shopping_lists (household_id, name) VALUES (?, ?)").bind(household).bind(s(l, "name").unwrap_or("")).execute(&mut **tx).await?.last_insert_rowid();
        remember(tx, "shopping_list", uuid, lid).await?;
        list_group.insert(uuid, group);
        report.shopping_lists += 1;
    }
    let item_recipes: HashMap<&str, &str> = b
        .rows("shopping_list_item_recipe_reference")
        .into_iter()
        .filter_map(|r| Some((s(r, "shopping_list_item_id")?, s(r, "recipe_id")?)))
        .collect();
    for i in b.rows("shopping_list_items") {
        let q = f(i, "quantity").filter(|q| *q > 0.0);
        let list_uuid = id(i, "shopping_list_id")?;
        let group = *list_group.get(list_uuid).ok_or_else(|| Error::Validation("backup refers to a shopping_list it does not contain".into()))?;
        let (unit, food) = (local_opt(tx, "unit", s(i, "unit_id")).await?, local_opt(tx, "food", s(i, "food_id")).await?);
        let label = local_opt(tx, "label", s(i, "label_id")).await?;
        let recipe = local_opt(tx, "recipe", item_recipes.get(id(i, "id")?).copied()).await?;
        for (table, what, found) in [("units", "unit", unit), ("foods", "food", food), ("recipes", "recipe", recipe), ("labels", "label", label)] {
            if let Some(x) = found {
                same_group(tx, table, x, group, &format!("shopping list item's {what}")).await?;
            }
        }
        let item_id = sqlx::query("INSERT INTO shopping_list_items (list_id, qty, unit_id, food_id, note, checked, recipe_id, label_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(local(tx, "shopping_list", list_uuid).await?)
            .bind(q)
            .bind(unit)
            .bind(food)
            .bind(s(i, "note").unwrap_or(""))
            .bind(flag(i, "checked"))
            .bind(recipe)
            .bind(label)
            .execute(&mut **tx)
            .await?
            .last_insert_rowid();
        remember(tx, "shopping_list_item", id(i, "id")?, item_id).await?;
        report.shopping_list_items += 1;
    }
    for p in b.rows("shopping_lists_multi_purpose_labels") {
        let list = local(tx, "shopping_list", id(p, "shopping_list_id")?).await?;
        let label = local(tx, "label", id(p, "label_id")?).await?;
        let group = *list_group.get(id(p, "shopping_list_id")?).ok_or_else(|| Error::Validation("backup refers to a shopping_list it does not contain".into()))?;
        same_group(tx, "labels", label, group, "shopping list's label order").await?;
        sqlx::query("INSERT OR REPLACE INTO shopping_list_labels (list_id, label_id, position) VALUES (?, ?, ?)").bind(list).bind(label).bind(p.get("position").and_then(Value::as_i64).unwrap_or(0)).execute(&mut **tx).await?;
    }
    // Extras (`key_name`, `value`; owner columns named after the model: unconfirmed, the fixture's tables are empty).
    for (uuid, extras) in extras_by_owner(b, "shopping_list_extras", &["shopping_list_id"]) {
        let list = local(tx, "shopping_list", &uuid).await?;
        sqlx::query("UPDATE shopping_lists SET extras = ? WHERE id = ?").bind(serde_json::to_string(&check_extras(extras)?)?).bind(list).execute(&mut **tx).await?;
    }
    for (uuid, extras) in extras_by_owner(b, "shopping_list_item_extras", &["shopping_list_item_id"]) {
        let item = local(tx, "shopping_list_item", &uuid).await?;
        sqlx::query("UPDATE shopping_list_items SET extras = ? WHERE id = ?").bind(serde_json::to_string(&check_extras(extras)?)?).bind(item).execute(&mut **tx).await?;
    }
    if !b.rows("shopping_list_recipe_reference").is_empty() {
        report.skipped.push("shopping lists' own recipe references are not imported; items keep theirs".into());
    }
    Ok(())
}

/// `YYYY-MM-DDTHH:MM:SS` or with a space, any fraction after it, read as UTC (Mealie's naive datetimes): `YYYY-MM-DDTHH:MM:SSZ`.
fn utc(ts: &str) -> Option<String> {
    let t = ts.get(..19)?;
    let digits = |r: std::ops::Range<usize>| t.as_bytes()[r].iter().all(u8::is_ascii_digit);
    let ok = digits(0..4) && digits(5..7) && digits(8..10) && digits(11..13) && digits(14..16) && digits(17..19) && &t[4..5] == "-" && &t[7..8] == "-" && matches!(&t[10..11], "T" | " ") && &t[13..14] == ":" && &t[16..17] == ":";
    ok.then(|| format!("{}T{}Z", &t[..10], &t[11..]))
}

/// Favourites and ratings, recipe actions, foods on hand, tools owned and share links. Column names are
/// from Mealie v3.28.0's models (the fixture's tables are empty): see `tests/import_part_b.rs`.
async fn import_household_features(tx: &mut Tx<'_>, b: &Backup, report: &mut ImportReport) -> Result<()> {
    // A favourite or a rating belongs to a user, and the recipe must be in the user's group.
    for r in b.rows("users_to_recipes") {
        let (user, recipe_uuid) = (local(tx, "user", id(r, "user_id")?).await?, id(r, "recipe_id")?);
        let recipe = local(tx, "recipe", recipe_uuid).await?;
        let same: Option<i64> = sqlx::query_scalar("SELECT 1 FROM recipes r, users u JOIN households h ON h.id = u.household_id WHERE r.id = ? AND u.id = ? AND r.group_id = h.group_id").bind(recipe).bind(user).fetch_optional(&mut **tx).await?;
        if same.is_none() {
            return Err(Error::Validation("backup favourite or rating pairs a user with a recipe of another group".into()));
        }
        let favourite = flag(r, "is_favorite");
        // Mealie writes 0 for "no rating"; its stars go in halves, and a stray value is brought to the nearest.
        let rating = f(r, "rating").filter(|v| v.is_finite() && *v > 0.0).map(|v| {
            let kept = ((v * 2.0).round() / 2.0).clamp(0.5, 5.0);
            if kept != v {
                report.skipped.push(format!("rating {v} of recipe {recipe_uuid} was kept as {kept}"));
            }
            kept
        });
        if !favourite && rating.is_none() {
            continue;
        }
        sqlx::query("INSERT INTO user_recipes (user_id, recipe_id, is_favorite, rating) VALUES (?, ?, ?, ?) ON CONFLICT (user_id, recipe_id) DO UPDATE SET is_favorite = excluded.is_favorite, rating = excluded.rating")
            .bind(user)
            .bind(recipe)
            .bind(favourite)
            .bind(rating)
            .execute(&mut **tx)
            .await?;
        report.favourites += usize::from(favourite);
        report.ratings += usize::from(rating.is_some());
    }

    // Written as they are: an admin's post action stays one, and a link that does not come out as http(s) is simply never shown.
    for r in b.rows("recipe_actions") {
        let group = local(tx, "group", id(r, "group_id")?).await?;
        let kind = s(r, "action_type").unwrap_or_default();
        if !["link", "post"].contains(&kind) {
            report.skipped.push(format!("recipe action {:?} has the type {kind:?}, which mealier does not have; not imported", s(r, "title").unwrap_or_default()));
            continue;
        }
        let household = match local_opt(tx, "household", s(r, "household_id")).await? {
            Some(h) => {
                same_group(tx, "households", h, group, "recipe action's household").await?;
                h
            }
            None => household_for(tx, None, group).await?,
        };
        sqlx::query("INSERT INTO recipe_actions (household_id, title, kind, url) VALUES (?, ?, ?, ?)").bind(household).bind(s(r, "title").unwrap_or_default()).bind(kind).bind(s(r, "url").unwrap_or_default()).execute(&mut **tx).await?;
        report.recipe_actions += 1;
    }

    for (table, kind, owner_table, col, link, is_food) in [
        ("households_to_ingredient_foods", "food", "foods", "food_id", "household_foods", true),
        ("households_to_tools", "tool", "tools", "tool_id", "household_tools", false),
    ] {
        for r in b.rows(table) {
            let household = local(tx, "household", id(r, "household_id")?).await?;
            let item = local(tx, kind, id(r, col)?).await?;
            let group: i64 = sqlx::query_scalar("SELECT group_id FROM households WHERE id = ?").bind(household).fetch_one(&mut **tx).await?;
            same_group(tx, owner_table, item, group, &format!("{kind} on a household")).await?;
            sqlx::query(&format!("INSERT OR IGNORE INTO {link} (household_id, {col}) VALUES (?, ?)")).bind(household).bind(item).execute(&mut **tx).await?;
            if is_food {
                report.foods_on_hand += 1;
            } else {
                report.tools_owned += 1;
            }
        }
    }

    // The token is Mealie's `id`; its URL spells it with dashes, so that is the form hashed.
    for r in b.rows("recipe_share_tokens") {
        let (group, recipe_uuid) = (local(tx, "group", id(r, "group_id")?).await?, id(r, "recipe_id")?);
        let recipe = local(tx, "recipe", recipe_uuid).await?;
        same_group(tx, "recipes", recipe, group, "share token's recipe").await?;
        let raw = id(r, "id")?;
        let Some(expires) = s(r, "expires_at").and_then(utc) else {
            report.skipped.push(format!("share token of recipe {recipe_uuid}: expiry {:?} is not a date; not imported", s(r, "expires_at").unwrap_or_default()));
            continue;
        };
        let token = if raw.len() == 32 && !raw.contains('-') { dashed(raw) } else { raw.to_string() };
        sqlx::query("INSERT INTO share_links (group_id, recipe_id, token_hash, created_at, expires_at) VALUES (?, ?, ?, COALESCE(?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now')), ?)")
            .bind(group)
            .bind(recipe)
            .bind(crate::hash_token(&token))
            .bind(s(r, "created_at").and_then(utc))
            .bind(expires)
            .execute(&mut **tx)
            .await?;
        report.share_links += 1;
    }
    Ok(())
}

fn dashed(uuid: &str) -> String {
    format!("{}-{}-{}-{}-{}", &uuid[..8], &uuid[8..12], &uuid[12..16], &uuid[16..20], &uuid[20..])
}

/// Each recipe's `original.<ext>`, put through the image module like any upload (so resized);
/// Mealie's thumbnails are regenerated later. Keyed by the recipe's Mealie id. A file that is not
/// a usable png, jpeg or webp is reported, not fatal. Done before the transaction, so decoding
/// does not hold it open, and so the recipes' snapshots can list the hashes.
async fn prepare_images(b: &Backup, archive: &mut ZipArchive<File>, report: &mut ImportReport) -> Result<HashMap<String, Processed>> {
    let mut out = HashMap::new();
    for r in b.rows("recipes") {
        let uuid = id(r, "id")?;
        if uuid.len() != 32 {
            continue;
        }
        let prefix = format!("data/recipes/{}/images/original.", dashed(uuid));
        let Some(name) = archive.file_names().filter_map(|n| n.ok()).find(|n| n.starts_with(&prefix)).map(|n| n.to_string()) else { continue };
        let mut bytes = Vec::new();
        archive.by_name(&name).map_err(archive_err)?.read_to_end(&mut bytes).map_err(archive_err)?;
        match crate::images::prepare(&bytes).await {
            Ok(p) => {
                out.insert(uuid.to_string(), p);
            }
            Err(e) => report.skipped.push(format!("image {name}: {e}")),
        }
    }
    Ok(out)
}

/// Records and links the prepared images. Returns each distinct one, to write once everything else has gone in.
async fn import_images(tx: &mut Tx<'_>, images: &HashMap<String, Processed>, report: &mut ImportReport) -> Result<Vec<Processed>> {
    let mut files: HashMap<&str, &Processed> = HashMap::new();
    for (uuid, photo) in images {
        photo.record(tx).await?;
        sqlx::query("INSERT INTO recipe_images (recipe_id, hash) VALUES (?, ?)")
            .bind(local(tx, "recipe", uuid).await?)
            .bind(&photo.hash)
            .execute(&mut **tx)
            .await?;
        files.insert(&photo.hash, photo);
    }
    report.images += files.len();
    Ok(files.into_values().cloned().collect())
}
