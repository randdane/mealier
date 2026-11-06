//! The Mealie importer's cookbooks and meal-plan rules (step 13): filters kept verbatim, parsed and
//! mapped to mealier ids; legacy tag/category/tool links merged in; unreadable filters reported.
mod common;
use std::path::{Path, PathBuf};

use mealier::{Db, ImportReport, Rng, Scope, import_mealie_backup};
use serde_json::{Value, json};

const TAG1: &str = "da133078-247a-42db-a599-57fe46062075";
const TAG2: &str = "890fe516-93e9-444e-944d-911946391437";
const CAT2: &str = "90bf843a-ba79-4c06-a2af-85b038946add";
const FIXTURE_GROUP: &str = "6a1a76f2eef844d7b079f8421d1d7b6d";
const FIXTURE_HOUSEHOLD: &str = "bdd39ab8b477457a89bcdf4d68e12d0d";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mealie_v3.28.0.zip")
}

/// The fixture with database.json edited by `edit`, as a new zip.
fn mutated(edit: impl Fn(&mut Value)) -> (tempfile::TempDir, PathBuf) {
    use std::io::{Read, Write};
    let mut src = zip::ZipArchive::new(std::fs::File::open(fixture()).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.zip");
    let mut out = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..src.len() {
        let mut f = src.by_index(i).unwrap();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).unwrap();
        let name = f.name().unwrap().to_string();
        if name == "database.json" {
            let mut v: Value = serde_json::from_slice(&bytes).unwrap();
            edit(&mut v);
            bytes = serde_json::to_vec(&v).unwrap();
        }
        out.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        out.write_all(&bytes).unwrap();
    }
    out.finish().unwrap();
    (dir, path)
}

fn push(v: &mut Value, table: &str, row: Value) {
    v[table].as_array_mut().unwrap().push(row);
}

async fn import(path: &Path) -> (Db, ImportReport, tempfile::TempDir) {
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, path, dir.path()).await.unwrap();
    (db, report, dir)
}

async fn tenant(db: &Db) -> Scope {
    let u = db.users().await.unwrap().into_iter().find(|u| u.username == "fixture-admin").unwrap();
    Scope::from(&u)
}

async fn names_in(db: &Db, sc: &Scope, cookbook: &str) -> Vec<String> {
    let c = db.cookbooks(sc).await.unwrap().into_iter().find(|c| c.name == cookbook).unwrap_or_else(|| panic!("no cookbook {cookbook}"));
    db.cookbook_recipes(sc, c.id).await.unwrap().into_iter().map(|r| r.name).collect()
}

fn skipped(report: &ImportReport, needle: &str) -> usize {
    report.skipped.iter().filter(|s| s.contains(needle)).count()
}

fn cookbook(id: &str, name: &str, filter: &str) -> Value {
    json!({"id": id, "name": name, "slug": name.to_lowercase(), "description": "", "position": 2, "public": false, "query_filter_string": filter,
           "group_id": FIXTURE_GROUP, "household_id": FIXTURE_HOUSEHOLD, "require_all_tags": true, "require_all_categories": true, "require_all_tools": true})
}

fn rule(id: &str, day: &str, entry: &str, filter: &str) -> Value {
    json!({"id": id, "group_id": FIXTURE_GROUP, "household_id": FIXTURE_HOUSEHOLD, "day": day, "entry_type": entry, "query_filter_string": filter})
}

#[tokio::test]
async fn the_fixtures_cookbook_keeps_its_filter_and_lists_the_recipe_the_tag_selects() {
    let (db, report, _dir) = import(&fixture()).await;
    let sc = tenant(&db).await;
    assert_eq!(report.cookbooks, 1);
    assert_eq!(skipped(&report, "cookbook"), 0, "{:?}", report.skipped);
    let c = db.cookbooks(&sc).await.unwrap().remove(0);
    assert_eq!((c.name.as_str(), c.description.as_str(), c.slug.as_str(), c.position, c.public, c.filter_supported), ("mealier-fixture-cookbook", "fixture", "mealier-fixture-cookbook", 1, false, true));
    assert_eq!(c.query_filter, format!(r#"tags.id IN ["{TAG1}"]"#), "Mealie's string, verbatim");
    assert_eq!(names_in(&db, &sc, "mealier-fixture-cookbook").await, ["mealier-fixture-recipe-a"]);
    // the structured form holds mealier's id for the tag, not Mealie's uuid
    let json: String = sqlx::query_scalar("SELECT filter_json FROM cookbooks WHERE id = ?").bind(c.id).fetch_one(db.pool()).await.unwrap();
    let tag: i64 = sqlx::query_scalar("SELECT id FROM tags WHERE name = 'mealier-fixture-tag-1'").fetch_one(db.pool()).await.unwrap();
    assert!(json.contains(&format!("\"{tag}\"")) && !json.contains("da133078"), "{json}");
}

#[tokio::test]
async fn richer_filters_are_mapped_and_evaluated() {
    let (_dir, path) = mutated(|v| {
        push(v, "cookbooks", cookbook("c2", "Both", &format!(r#"tags.id CONTAINS ALL ["{TAG1}", "{TAG2}"] AND recipe_category.id IN ["{CAT2}"]"#)));
        push(v, "cookbooks", cookbook("c3", "Not made", &format!(r#"(tags.id IN ["{TAG1}"] OR name LIKE "%nothing%") AND lastMade IS NULL"#)));
        push(v, "cookbooks", cookbook("c4", "By name", r#"name LIKE "%recipe-b%" OR description = "mealier importer fixture""#));
        push(v, "cookbooks", cookbook("c5", "Without", &format!(r#"tags.id NOT IN ["{TAG1}"]"#)));
        push(v, "cookbooks", cookbook("c6", "All", ""));
    });
    let (db, report, _d) = import(&path).await;
    let sc = tenant(&db).await;
    assert_eq!(report.cookbooks, 6);
    assert_eq!(skipped(&report, "cookbook"), 0, "{:?}", report.skipped);
    assert_eq!(names_in(&db, &sc, "Both").await, ["mealier-fixture-recipe-a"]);
    // recipe-a has a made-it event in the fixture, so lastMade is not null
    assert_eq!(names_in(&db, &sc, "Not made").await, Vec::<String>::new());
    assert_eq!(names_in(&db, &sc, "By name").await.len(), 2);
    assert_eq!(names_in(&db, &sc, "Without").await, ["mealier-fixture-recipe-b"]);
    assert_eq!(names_in(&db, &sc, "All").await.len(), 2);
    let all = db.cookbooks(&sc).await.unwrap();
    assert!(all.iter().all(|c| c.filter_supported));
}

#[tokio::test]
async fn an_unreadable_filter_is_kept_verbatim_reported_and_lists_nothing() {
    let (_dir, path) = mutated(|v| {
        push(v, "cookbooks", cookbook("c2", "Odd", r#"household_id = "abc" AND tags.id IN ["x"]"#));
        push(v, "cookbooks", cookbook("c3", "Oddly", r#"recipe_ingredient.food.id IN ["x"]"#));
    });
    let (db, report, _d) = import(&path).await;
    let sc = tenant(&db).await;
    assert_eq!(report.cookbooks, 3, "the cookbooks are imported all the same");
    assert_eq!(skipped(&report, "Odd"), 2, "one line each: {:?}", report.skipped); // "Odd" and "Oddly"
    let line = report.skipped.iter().find(|s| s.contains("\"Odd\"")).unwrap();
    assert!(line.contains("household_id") && line.contains("not supported"), "{line}");
    let c = db.cookbooks(&sc).await.unwrap().into_iter().find(|c| c.name == "Odd").unwrap();
    assert_eq!((c.query_filter.as_str(), c.filter_supported), (r#"household_id = "abc" AND tags.id IN ["x"]"#, false));
    let json: Option<String> = sqlx::query_scalar("SELECT filter_json FROM cookbooks WHERE id = ?").bind(c.id).fetch_one(db.pool()).await.unwrap();
    assert_eq!(json, None);
    assert!(db.cookbook_recipes(&sc, c.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_id_the_backup_does_not_contain_matches_nothing_and_is_reported() {
    let (_dir, path) = mutated(|v| {
        push(v, "cookbooks", cookbook("c2", "Ghost", r#"tags.id IN ["11111111-2222-3333-4444-555555555555"]"#));
        push(v, "cookbooks", cookbook("c3", "Not ghost", r#"tags.id NOT IN ["11111111-2222-3333-4444-555555555555"]"#));
    });
    let (db, report, _d) = import(&path).await;
    let sc = tenant(&db).await;
    assert_eq!(skipped(&report, "11111111-2222-3333-4444-555555555555"), 2, "{:?}", report.skipped);
    assert!(names_in(&db, &sc, "Ghost").await.is_empty());
    assert_eq!(names_in(&db, &sc, "Not ghost").await.len(), 2);
}

#[tokio::test]
async fn legacy_tag_category_and_tool_links_are_merged_into_the_filter() {
    let (_dir, path) = mutated(|v| {
        push(v, "cookbooks", cookbook("c2", "Linked all", ""));
        push(v, "cookbooks_to_tags", json!({"cookbook_id": "c2", "tag_id": TAG1.replace('-', "")}));
        push(v, "cookbooks_to_tags", json!({"cookbook_id": "c2", "tag_id": TAG2.replace('-', "")}));
        let mut any = cookbook("c3", "Linked any", r#"name LIKE "%recipe-a%""#);
        any["require_all_tags"] = json!(false);
        any["require_all_tools"] = json!(false);
        push(v, "cookbooks", any);
        push(v, "cookbooks_to_tags", json!({"cookbook_id": "c3", "tag_id": TAG2.replace('-', "")}));
        push(v, "cookbooks_to_tools", json!({"cookbook_id": "c3", "tool_id": "9a154a53ffd74a9b907c4199e14616c1"}));
    });
    let (db, _report, _d) = import(&path).await;
    let sc = tenant(&db).await;
    assert_eq!(names_in(&db, &sc, "Linked all").await, ["mealier-fixture-recipe-a"]);
    assert_eq!(names_in(&db, &sc, "Linked any").await, ["mealier-fixture-recipe-a"]);
    let c = db.cookbooks(&sc).await.unwrap().into_iter().find(|c| c.name == "Linked all").unwrap();
    assert!(c.query_filter.contains("tags.id CONTAINS ALL") && c.filter_supported, "the links are in the string too: {}", c.query_filter);
    let c = db.cookbooks(&sc).await.unwrap().into_iter().find(|c| c.name == "Linked any").unwrap();
    assert!(c.query_filter.starts_with("(name LIKE") && c.query_filter.contains("tags.id IN") && c.query_filter.contains("tools.id IN"), "{}", c.query_filter);
}

#[tokio::test]
async fn a_cookbook_filter_naming_another_groups_tag_is_refused_whole() {
    let (_dir, path) = mutated(|v| {
        push(v, "groups", json!({"id": "g2", "name": "Second", "slug": "second"}));
        push(v, "households", json!({"id": "h2", "group_id": "g2", "name": "Second Home", "slug": "second-home"}));
        push(v, "tags", json!({"id": "00000000000000000000000000000t02", "name": "theirs", "group_id": "g2"}));
        push(v, "cookbooks", cookbook("c2", "Sneaky", r#"tags.id IN ["00000000-0000-0000-0000-000000000t02"]"#));
    });
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = import_mealie_backup(&db, &path, dir.path()).await.unwrap_err();
    assert!(matches!(&err, mealier::Error::Validation(m) if m.contains("another group")), "{err}");
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM cookbooks").fetch_one(db.pool()).await.unwrap(), 0, "nothing was kept");
}

// ---------- meal-plan rules ----------

#[tokio::test]
async fn rules_come_over_with_day_slot_filter_and_households_and_suggest_from_them() {
    let (_dir, path) = mutated(|v| {
        push(v, "group_meal_plan_rules", rule("r1", "monday", "dinner", &format!(r#"tags.id IN ["{TAG1}"]"#)));
        push(v, "plan_rules_to_households", json!({"group_plan_rule_id": "r1", "household_id": FIXTURE_HOUSEHOLD}));
        push(v, "group_meal_plan_rules", rule("r2", "unset", "unset", ""));
        push(v, "group_meal_plan_rules", rule("r3", "unset", "lunch", ""));
        push(v, "plan_rules_to_tags", json!({"group_plan_rule_id": "r3", "tag_id": TAG2.replace('-', "")}));
        push(v, "plan_rules_to_categories", json!({"group_plan_rule_id": "r3", "category_id": CAT2.replace('-', "")}));
    });
    let (db, report, _d) = import(&path).await;
    let sc = tenant(&db).await;
    assert_eq!(report.meal_plan_rules, 3);
    assert_eq!(skipped(&report, "rule"), 0, "{:?}", report.skipped);
    let rules = db.rules(&sc).await.unwrap();
    assert_eq!(rules.len(), 3);
    let r1 = &rules[0];
    assert_eq!((r1.day.as_str(), r1.slot.as_str(), r1.query_filter.as_str(), r1.filter_supported, r1.household_ids.as_slice()), ("monday", "dinner", format!(r#"tags.id IN ["{TAG1}"]"#).as_str(), true, &[sc.household_id][..]));
    assert_eq!((rules[1].day.as_str(), rules[1].slot.as_str(), rules[1].household_ids.len()), ("unset", "unset", 0));
    assert!(rules[2].query_filter.contains("tags.id IN") && rules[2].query_filter.contains("recipe_category.id IN"), "legacy links merged: {}", rules[2].query_filter);

    // Monday 2026-11-02 dinner: the most specific rule, tag 1, which only recipe-a carries
    let pick = db.suggest(&sc, "2026-11-02", "dinner", &mut Rng::seeded(1)).await.unwrap().unwrap();
    assert_eq!(pick.name, "mealier-fixture-recipe-a");
    // Tuesday lunch: slot rule r3 (tag 2 and category 2, which recipe-a has too)
    let pick = db.suggest(&sc, "2026-11-03", "lunch", &mut Rng::seeded(1)).await.unwrap().unwrap();
    assert_eq!(pick.name, "mealier-fixture-recipe-a");
    // Tuesday dinner: only the catch-all, so either recipe
    let mut seen = std::collections::BTreeSet::new();
    for seed in 0..30 {
        seen.insert(db.suggest(&sc, "2026-11-03", "dinner", &mut Rng::seeded(seed)).await.unwrap().unwrap().name);
    }
    assert_eq!(seen.len(), 2, "{seen:?}");
}

#[tokio::test]
async fn an_unreadable_rule_filter_or_a_bad_day_is_reported() {
    let (_dir, path) = mutated(|v| {
        push(v, "group_meal_plan_rules", rule("r1", "monday", "dinner", "household_id = \"x\""));
        push(v, "group_meal_plan_rules", rule("r2", "someday", "dinner", ""));
        push(v, "group_meal_plan_rules", rule("r3", "monday", "brunch", ""));
    });
    let (db, report, _d) = import(&path).await;
    let sc = tenant(&db).await;
    assert_eq!(report.meal_plan_rules, 1, "the two with a day or meal mealier does not have are skipped");
    assert_eq!(skipped(&report, "someday"), 1, "{:?}", report.skipped);
    assert_eq!(skipped(&report, "brunch"), 1, "{:?}", report.skipped);
    let line = report.skipped.iter().find(|s| s.contains("household_id")).unwrap();
    assert!(line.contains("rule") && line.contains("not supported"), "{line}");
    let r = db.rules(&sc).await.unwrap().remove(0);
    assert_eq!((r.query_filter.as_str(), r.filter_supported), ("household_id = \"x\"", false));
    assert_eq!(db.suggest(&sc, "2026-11-02", "dinner", &mut Rng::seeded(1)).await.unwrap(), None, "a rule with an unreadable filter selects nothing");
}

#[tokio::test]
async fn a_rule_naming_another_groups_household_is_refused_whole() {
    let (_dir, path) = mutated(|v| {
        push(v, "groups", json!({"id": "g2", "name": "Second", "slug": "second"}));
        push(v, "households", json!({"id": "h2", "group_id": "g2", "name": "Second Home", "slug": "second-home"}));
        push(v, "group_meal_plan_rules", rule("r1", "unset", "unset", ""));
        push(v, "plan_rules_to_households", json!({"group_plan_rule_id": "r1", "household_id": "h2"}));
    });
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = import_mealie_backup(&db, &path, dir.path()).await.unwrap_err();
    assert!(matches!(&err, mealier::Error::Validation(m) if m.contains("another group")), "{err}");
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM meal_plan_rules").fetch_one(db.pool()).await.unwrap(), 0);
}
