//! Step 16: the importer against REAL rows. Every table below was filled in production Mealie v3.28.0 by
//! `scripts/seed-mealie-fixture.py` (through its API) and scrubbed by `scripts/make-mealie-fixture.py`, so
//! the column names these tests rely on are Mealie's own, not guesses. The two exceptions are the legacy
//! link tables `cookbooks_to_*` and `plan_rules_to_*`: Mealie v3.28's API cannot fill them, so their tests
//! (in `import_cookbooks.rs`) inject rows, with column names read from Mealie's models.
mod common;
use std::path::{Path, PathBuf};

use mealier::{Db, ImportReport, Recipe, Rng, Scope, import_mealie_backup};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mealie_v3.28.0.zip")
}

async fn import() -> (Db, ImportReport, tempfile::TempDir) {
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, &fixture(), dir.path()).await.unwrap();
    (db, report, dir)
}

async fn scope_of(db: &Db, name: &str) -> Scope {
    Scope::from(&db.users().await.unwrap().into_iter().find(|u| u.username == name).unwrap())
}

async fn recipe(db: &Db, name: &str) -> Recipe {
    let sc = scope_of(db, "fixture-admin").await;
    let id = db.list_recipes(&sc, true, None).await.unwrap().into_iter().find(|r| r.name == name).unwrap().id;
    db.get_recipe(&sc, id).await.unwrap().unwrap()
}

fn skipped(report: &ImportReport, needle: &str) -> usize {
    report.skipped.iter().filter(|s| s.contains(needle)).count()
}

async fn count(db: &Db, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap()
}

#[tokio::test]
async fn report_counts_for_every_kind_the_fixture_holds() {
    let (_db, r, _d) = import().await;
    assert_eq!((r.groups, r.households, r.users), (2, 2, 2));
    assert_eq!((r.recipes, r.cookbooks, r.meal_plan_rules, r.meal_plan_entries), (3, 3, 2, 3));
    assert_eq!((r.shopping_lists, r.shopping_list_items), (2, 5));
    assert_eq!((r.favourites, r.ratings, r.recipe_actions, r.foods_on_hand, r.tools_owned, r.share_links, r.assets), (1, 1, 2, 1, 1, 1, 1));
    assert_eq!((r.webhooks, r.scheduled_webhooks), (2, 1), "ntfy and json notifiers; the discord one is reported");
}

// ---------- recipe C: sections, step links, substitutions, notes, extras, asset, settings ----------

const REF: [&str; 3] = ["fad0e57d17864fd1a92b893391400d0f", "6e5cf001a2ae413fa902c5a3b038aa01", "18a787802dfd4658b6fe87154577371c"];

#[tokio::test]
async fn recipe_c_keeps_sections_step_links_substitutions_notes_extras_and_settings() {
    let (db, report, _d) = import().await;
    let c = recipe(&db, "mealier-fixture-recipe-c").await.content;
    assert_eq!(c.ingredients.iter().map(|i| (i.uid.as_str(), i.section.as_str())).collect::<Vec<_>>(), [
        (REF[0], "mealier-fixture section: dough"),
        (REF[1], ""),
        (REF[2], "mealier-fixture section: wet"),
    ]);
    assert_eq!(c.step_details.iter().map(|d| (d.title.as_str(), d.ingredient_uids.as_slice())).collect::<Vec<_>>(), [
        ("mealier-fixture step title", &[REF[0].to_string(), REF[1].to_string()][..]),
        ("", &[REF[2].to_string()][..]),
    ]);
    // sugar has a food substitute with a note, and a note-only one
    let subs: Vec<_> = c.ingredients[1].substitutions.iter().map(|s| (s.food.as_deref(), s.note.as_str())).collect();
    assert_eq!(subs, [(Some("mealier-fixture-honey"), "mealier-fixture honey, less"), (None, "mealier-fixture or maple syrup")]);
    assert!(c.ingredients[0].substitutions.is_empty() && c.ingredients[2].substitutions.is_empty());
    assert_eq!(c.notes.iter().map(|n| (n.title.as_str(), n.text.as_str())).collect::<Vec<_>>(), [("mealier-fixture note", "a recipe note")]);
    assert_eq!(c.extras.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(), [("fixture_recipe_key", "fixture recipe value"), ("fixture_second_key", "2")]);
    let s = &c.settings;
    assert_eq!((s.locked, s.disable_comments, s.hide_nutrition, s.public, s.disable_amount, s.show_assets, s.landscape_view), (true, true, true, true, true, false, false));
    assert_eq!(c.tools, ["mealier-fixture-tool"]);
    assert_eq!(c.tags, ["mealier-fixture-tag-2"]);
    assert_eq!(skipped(&report, "1 step summaries"), 1, "{:?}", report.skipped);
    assert_eq!(skipped(&report, "1 note references"), 1, "{:?}", report.skipped);
}

#[tokio::test]
async fn recipe_c_has_its_asset_on_disk_byte_for_byte() {
    let (db, _report, dir) = import().await;
    let sc = scope_of(&db, "fixture-admin").await;
    let c = recipe(&db, "mealier-fixture-recipe-c").await;
    let assets = db.recipe_assets(&sc, c.id).await.unwrap();
    assert_eq!(assets.iter().map(|a| (a.name.as_str(), a.icon.as_str(), a.file_name.as_str())).collect::<Vec<_>>(), [("mealier-fixture asset", "mdi-file", "mealier-fixture-asset.txt")]);
    assert_eq!(std::fs::read(dir.path().join("assets").join(&assets[0].hash)).unwrap(), b"mealier fixture asset\n");
    assert_eq!(c.content.assets, [assets[0].hash.clone()]);
}

// ---------- foods: extras, substitutions, on hand ----------

#[tokio::test]
async fn food_extras_and_substitutions_and_what_is_on_hand() {
    let (db, report, _d) = import().await;
    let sc = scope_of(&db, "fixture-admin").await;
    let foods = db.foods(&sc).await.unwrap();
    let id = |n: &str| foods.iter().find(|f| f.name == n).unwrap().id;
    let sugar = db.food(&sc, id("mealier-fixture-sugar")).await.unwrap();
    assert_eq!(sugar.extras.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(), [("fixture_food_key", "fixture food value")]);
    assert_eq!(sugar.substitutions.iter().map(|s| (s.name.as_str(), s.note.as_str())).collect::<Vec<_>>(), [("mealier-fixture-honey", "mealier-fixture use 3/4 as much")]);
    assert_eq!(skipped(&report, "has no substitute food"), 1, "a note-only food substitution cannot be kept: {:?}", report.skipped);

    let p = db.pantry(&sc).await.unwrap();
    let have = |items: &[mealier::PantryItem]| items.iter().filter(|i| i.have).map(|i| i.name.clone()).collect::<Vec<_>>();
    assert_eq!(have(&p.foods), ["mealier-fixture-flour"]);
    assert_eq!(have(&p.tools), ["mealier-fixture-tool"]);
}

// ---------- shopping lists: extras, and the second household's own ----------

#[tokio::test]
async fn shopping_list_and_item_extras_come_across() {
    let (db, _report, _d) = import().await;
    let sc = scope_of(&db, "fixture-admin").await;
    let lists = db.shopping_lists(&sc).await.unwrap();
    assert_eq!(lists.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["mealier-fixture-list"]);
    let list = db.shopping_list(&sc, lists[0].id).await.unwrap();
    assert_eq!(list.extras.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(), [("fixture_list_key", "fixture list value")]);
    let with: Vec<_> = list.items.iter().filter(|i| !i.extras.is_empty()).map(|i| (i.note.as_str(), i.extras.get("fixture_item_key").map(String::as_str))).collect();
    assert_eq!(with, [("hand added", Some("fixture item value"))]);
}

#[tokio::test]
async fn the_second_group_holds_the_second_user_with_its_own_plan_and_list() {
    let (db, _report, _d) = import().await;
    let groups = db.groups().await.unwrap();
    assert_eq!(groups.iter().map(|g| (g.name.as_str(), g.slug.as_str())).collect::<Vec<_>>(), [("Fixture", "fixture"), ("Fixture Two", "fixture-two")]);
    let households = db.households().await.unwrap();
    assert_eq!(households.iter().map(|h| (h.slug.as_str(), h.group_id)).collect::<Vec<_>>(), [("fixture", groups[0].id), ("fixture-two", groups[1].id)]);
    assert_eq!(db.default_household().await.unwrap().slug, "fixture");
    let (admin, user) = (scope_of(&db, "fixture-admin").await, scope_of(&db, "fixture-user").await);
    assert_eq!((admin.group_id, user.group_id, user.household_id), (groups[0].id, groups[1].id, households[1].id));
    let prefs: Vec<(bool, i64)> = sqlx::query_as("SELECT private_group, group_id FROM group_preferences ORDER BY group_id").fetch_all(db.pool()).await.unwrap();
    assert_eq!(prefs, [(false, groups[0].id), (true, groups[1].id)], "the second group is private");

    let plan = db.meal_plan(&user, "2026-12-01", "2026-12-31").await.unwrap();
    assert_eq!(plan.iter().map(|e| (e.date.as_str(), e.slot.as_str(), e.title.as_str(), e.text.as_str(), e.recipe_id)).collect::<Vec<_>>(), [("2026-12-05", "dinner", "mealier-fixture g2 note entry", "second household", None)]);
    let lists = db.shopping_lists(&user).await.unwrap();
    assert_eq!(lists.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["mealier-fixture-list-g2"]);
    let list = db.shopping_list(&user, lists[0].id).await.unwrap();
    assert_eq!(list.items.iter().map(|i| (i.note.as_str(), i.qty)).collect::<Vec<_>>(), [("mealier-fixture g2 item", Some(2.0))]);
    // and nothing leaks across
    assert!(db.list_recipes(&user, true, None).await.unwrap().is_empty());
    assert_eq!(db.meal_plan(&admin, "2026-12-01", "2026-12-31").await.unwrap().len(), 2);
}

// ---------- favourites, ratings, actions, share links ----------

#[tokio::test]
async fn the_admins_favourite_and_rating_of_recipe_c() {
    let (db, _report, _d) = import().await;
    let admin = scope_of(&db, "fixture-admin").await;
    let c = recipe(&db, "mealier-fixture-recipe-c").await;
    let o = db.opinion(&admin, c.id).await.unwrap();
    assert_eq!((o.favourite, o.rating), (true, Some(4.0)));
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    assert_eq!((db.opinion(&admin, a.id).await.unwrap().favourite, db.opinion(&admin, a.id).await.unwrap().rating), (false, None));
    assert_eq!(count(&db, "SELECT count(*) FROM user_recipes").await, 1);
}

#[tokio::test]
async fn recipe_actions_keep_their_kind_url_and_household() {
    let (db, _report, _d) = import().await;
    let sc = scope_of(&db, "fixture-admin").await;
    let got = db.actions(&sc).await.unwrap();
    assert_eq!(got.iter().map(|a| (a.title.as_str(), a.kind.as_str(), a.url.as_str())).collect::<Vec<_>>(), [
        ("mealier-fixture-action-link", "link", "https://example.invalid/mealier-fixture/link"),
        ("mealier-fixture-action-post", "post", "https://example.invalid/mealier-fixture"),
    ]);
    assert!(db.actions(&scope_of(&db, "fixture-user").await).await.unwrap().is_empty(), "household-scoped");
}

#[tokio::test]
async fn the_share_token_still_opens_recipe_c_and_only_its_hash_is_stored() {
    let (db, _report, _d) = import().await;
    // the committed fixture carries a fake token (32 hex digits); Mealie shows it with dashes
    let shared = db.shared_recipe("fa4efa4e-fa4e-fa4e-fa4e-fa4efa4efa4e").await.unwrap();
    assert_eq!(shared.content.name, "mealier-fixture-recipe-c");
    assert!(db.shared_recipe("fa4efa4efa4efa4efa4efa4efa4efa4e").await.is_err(), "the dashed form is the token");
    let links = db.share_links(&scope_of(&db, "fixture-admin").await, shared.id).await.unwrap();
    assert_eq!(links.iter().map(|l| l.expires_at.as_str()).collect::<Vec<_>>(), ["2036-01-01T00:00:00Z"]);
    let all: String = sqlx::query_scalar("SELECT json_group_array(json_array(token_hash, created_at, expires_at)) FROM share_links").fetch_one(db.pool()).await.unwrap();
    assert!(!all.contains("fa4efa4e"));
}

// ---------- cookbooks and meal-plan rules ----------

async fn names_in(db: &Db, sc: &Scope, cookbook: &str) -> Vec<String> {
    let c = db.cookbooks(sc).await.unwrap().into_iter().find(|c| c.name == cookbook).unwrap_or_else(|| panic!("no cookbook {cookbook}"));
    db.cookbook_recipes(sc, c.id).await.unwrap().into_iter().map(|r| r.name).collect()
}

#[tokio::test]
async fn cookbooks_with_tags_categories_and_tools_in_any_and_all_variations() {
    let (db, report, _d) = import().await;
    let sc = scope_of(&db, "fixture-admin").await;
    assert!(db.cookbooks(&sc).await.unwrap().iter().all(|c| c.filter_supported), "{:?}", report.skipped);
    assert_eq!(skipped(&report, "cookbook"), 0, "{:?}", report.skipped);
    // any of: recipe c has tag 2, category 1 and the tool, so it matches; all of: it lacks tag 1 and category 2
    assert_eq!(names_in(&db, &sc, "mealier-fixture-cookbook-any").await, ["mealier-fixture-recipe-a", "mealier-fixture-recipe-c"]);
    assert_eq!(names_in(&db, &sc, "mealier-fixture-cookbook-all").await, ["mealier-fixture-recipe-a"]);
    let all = db.cookbooks(&sc).await.unwrap().into_iter().find(|c| c.name == "mealier-fixture-cookbook-all").unwrap();
    assert_eq!((all.position, all.public, all.description.as_str()), (3, true, "tags/categories/tools, all of"));
    assert!(all.query_filter.contains("tags.id CONTAINS ALL") && all.query_filter.contains("tools.id CONTAINS ALL"), "{}", all.query_filter);
}

#[tokio::test]
async fn rules_keep_day_slot_filter_and_the_household_they_belong_to() {
    let (db, report, _d) = import().await;
    let sc = scope_of(&db, "fixture-admin").await;
    assert_eq!(skipped(&report, "rule"), 0, "{:?}", report.skipped);
    let rules = db.rules(&sc).await.unwrap();
    assert_eq!(rules.iter().map(|r| (r.day.as_str(), r.slot.as_str(), r.filter_supported)).collect::<Vec<_>>(), [("monday", "dinner", true), ("unset", "unset", true)]);
    assert!(rules[0].query_filter.starts_with("tags.id IN") && rules[1].query_filter.starts_with("recipe_category.id IN"));
    // `group_meal_plan_rules.household_id` is the household the rule belongs to
    assert_eq!(rules.iter().map(|r| r.household_ids.clone()).collect::<Vec<_>>(), [vec![sc.household_id], vec![sc.household_id]]);
    // Monday 2026-11-02 dinner: the monday rule, tag 1, which only recipe a has
    assert_eq!(db.suggest(&sc, "2026-11-02", "dinner", &mut Rng::seeded(1)).await.unwrap().unwrap().name, "mealier-fixture-recipe-a");
    // the second group's household has no rules, and cannot borrow the first's
    let other = scope_of(&db, "fixture-user").await;
    assert!(db.rules(&other).await.unwrap().is_empty());
}

// ---------- notifiers and the scheduled webhook ----------

#[tokio::test]
async fn notifiers_and_the_scheduled_webhook() {
    let (db, report, _d) = import().await;
    let hooks = db.list_webhooks().await.unwrap();
    let by = |n: &str| hooks.iter().find(|h| h.name == n).unwrap_or_else(|| panic!("no webhook {n}"));
    let ntfy = by("mealier-fixture-notifier");
    assert_eq!((ntfy.kind.as_str(), ntfy.url.as_str(), ntfy.enabled), ("ntfy", "https://ntfy.example.invalid/mealier-fixture", true));
    assert_eq!(ntfy.events, ["recipe_created", "recipe_updated", "mealplan_entry_created", "shopping_list_created", "cookbook_deleted"]);
    let json = by("mealier-fixture-notifier-json");
    assert_eq!((json.kind.as_str(), json.url.as_str(), json.enabled), ("json", "http://example.invalid/mealier-fixture", false));
    assert_eq!(json.events, ["recipe_deleted", "shopping_list_updated", "shopping_list_deleted", "cookbook_created"]);
    let groups = db.groups().await.unwrap();
    assert_eq!((ntfy.group_id, json.group_id), (Some(groups[0].id), Some(groups[0].id)));

    // the options mealier has no event for are named on one line, and the discord notifier on another
    let line = report.skipped.iter().find(|s| s.starts_with("notifier \"mealier-fixture-notifier\"")).expect("a line for the ntfy notifier");
    for o in ["user_signup", "data_import", "tag_created", "label_deleted"] {
        assert!(line.contains(o), "{o}: {line}");
    }
    let discord: Vec<&String> = report.skipped.iter().filter(|s| s.contains("mealier-fixture-notifier-discord")).collect();
    assert_eq!(discord.len(), 1, "{:?}", report.skipped);
    assert!(discord[0].contains("discord") && !discord[0].contains("0000"), "{}", discord[0]);

    let sched = by("mealier-fixture-webhook");
    assert_eq!((sched.kind.as_str(), sched.url.as_str(), sched.enabled, sched.time_of_day.as_deref()), ("scheduled", "https://example.invalid/mealier-fixture", false, Some("07:30")));
    assert_eq!((sched.group_id, sched.household_id), (Some(groups[0].id), Some(db.default_household().await.unwrap().id)));
}

// ---------- the whole backup ----------

/// The spec's "Skipped on purpose" list (Migration from Mealie): secrets, one-time links, job history, the
/// version table, AI providers, and Mealie's unused group-category link.
const SKIPPED_ON_PURPOSE: [&str; 12] = [
    "long_live_tokens", "invite_tokens", "password_reset_tokens", "server_tasks", "group_reports", "report_entries", "group_data_exports", "alembic_version",
    "ai_providers", "ai_provider_settings", "ai_provider_headers", "ai_provider_params",
];

/// Tables mealier does not store but reads, to say so in the report (the spec's "read only for a report line").
const REPORTED_ONLY: [&str; 3] = ["recipe_note_ref_link", "shopping_list_recipe_reference", "households_to_recipes"];

/// Legacy link tables Mealie v3.28's API cannot fill, so the fixture holds none; `import_cookbooks.rs` injects rows.
const LEGACY_LINKS: [&str; 7] = ["cookbooks_to_categories", "cookbooks_to_tags", "cookbooks_to_tools", "plan_rules_to_tags", "plan_rules_to_categories", "plan_rules_to_households", "group_to_categories"];

/// (Mealie table, SQL counting what it became in mealier, expected count, must equal the Mealie row count)
const IMPORTED: [(&str, &str, i64, bool); 33] = [
    ("groups", "SELECT count(*) FROM groups", 2, true),
    ("group_preferences", "SELECT count(*) FROM group_preferences", 2, true),
    ("households", "SELECT count(*) FROM households", 2, true),
    ("household_preferences", "SELECT count(*) FROM household_preferences", 2, true),
    ("users", "SELECT count(*) FROM users WHERE local_admin = 0", 2, true),
    ("ingredient_units", "SELECT count(*) FROM units", 26, true),
    ("ingredient_units_aliases", "SELECT count(*) FROM units WHERE aliases <> '[]'", 1, true),
    ("multi_purpose_labels", "SELECT count(*) FROM labels", 33, true),
    ("ingredient_foods", "SELECT count(*) FROM foods", 2691, true),
    ("ingredient_foods_aliases", "SELECT count(*) FROM foods WHERE aliases <> '[]'", 1, true),
    ("ingredient_food_extras", "SELECT count(*) FROM foods WHERE extras <> '{}'", 1, true),
    ("ingredient_foods_substitutions", "SELECT count(*) FROM food_substitutions", 1, false), // the note-only one is reported
    ("households_to_ingredient_foods", "SELECT count(*) FROM household_foods", 1, true),
    ("households_to_tools", "SELECT count(*) FROM household_tools", 1, true),
    ("tags", "SELECT count(*) FROM tags", 2, true),
    ("categories", "SELECT count(*) FROM categories", 2, true),
    ("tools", "SELECT count(*) FROM tools", 1, true),
    ("recipes", "SELECT count(*) FROM recipes", 3, true),
    ("recipe_assets", "SELECT count(*) FROM recipe_assets", 1, true),
    ("recipe_timeline_events", "SELECT count(*) FROM batches", 1, false), // only "made it" events; the rest are reported
    ("recipe_comments", "SELECT count(*) FROM recipe_comments", 1, true),
    ("group_meal_plans", "SELECT count(*) FROM meal_plan_entries", 3, true),
    ("cookbooks", "SELECT count(*) FROM cookbooks", 3, true),
    ("group_meal_plan_rules", "SELECT count(*) FROM meal_plan_rules", 2, true),
    ("shopping_lists", "SELECT count(*) FROM shopping_lists", 2, true),
    ("shopping_list_items", "SELECT count(*) FROM shopping_list_items", 5, true),
    ("shopping_list_item_recipe_reference", "SELECT count(*) FROM shopping_list_items WHERE recipe_id IS NOT NULL", 1, true),
    ("shopping_lists_multi_purpose_labels", "SELECT count(*) FROM shopping_list_labels", 33, true),
    ("shopping_list_extras", "SELECT count(*) FROM shopping_lists WHERE extras <> '{}'", 1, true),
    ("shopping_list_item_extras", "SELECT count(*) FROM shopping_list_items WHERE extras <> '{}'", 1, true),
    ("users_to_recipes", "SELECT count(*) FROM user_recipes", 1, true),
    ("recipe_actions", "SELECT count(*) FROM recipe_actions", 2, true),
    ("recipe_share_tokens", "SELECT count(*) FROM share_links", 1, true),
];

/// Tables whose rows live inside a recipe's content: (Mealie table, how many that content holds across the recipes).
type Part = (&'static str, fn(&Recipe) -> usize);
const RECIPE_PARTS: [Part; 14] = [
    ("recipes_to_tags", |r| r.content.tags.len()),
    ("recipes_to_categories", |r| r.content.categories.len()),
    ("recipes_to_tools", |r| r.content.tools.len()),
    ("recipes_ingredients", |r| r.content.ingredients.len()),
    ("recipes_ingredients_substitutions", |r| r.content.ingredients.iter().map(|i| i.substitutions.len()).sum()),
    ("recipe_instructions", |r| r.content.steps.len()),
    ("recipe_ingredient_ref_link", |r| r.content.step_details.iter().map(|d| d.ingredient_uids.len()).sum()),
    ("recipe_nutrition", |r| usize::from(r.content.nutrition.is_some())),
    ("recipe_settings", |_| 1),
    ("notes", |r| r.content.notes.len()),
    ("api_extras", |r| r.content.extras.len()),
    ("webhook_urls", |_| 0),
    ("group_events_notifiers", |_| 0),
    ("group_events_notifier_options", |_| 0),
];

#[tokio::test]
async fn every_table_in_the_backup_is_imported_or_skipped_on_purpose_and_none_is_unknown() {
    let json: serde_json::Value = {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut zip::ZipArchive::new(std::fs::File::open(fixture()).unwrap()).unwrap().by_name("database.json").unwrap(), &mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    };
    let (db, report, _d) = import().await;
    assert_eq!(skipped(&report, "unknown table"), 0, "{:?}", report.skipped);

    let sc = scope_of(&db, "fixture-admin").await;
    let mut recipes = Vec::new();
    for r in db.list_recipes(&sc, true, None).await.unwrap() {
        recipes.push(db.get_recipe(&sc, r.id).await.unwrap().unwrap());
    }
    let rows = |t: &str| json[t].as_array().map_or(0, Vec::len) as i64;

    for (table, data) in json.as_object().unwrap() {
        let n = data.as_array().unwrap().len() as i64;
        let t = table.as_str();
        if SKIPPED_ON_PURPOSE.contains(&t) || LEGACY_LINKS.contains(&t) || REPORTED_ONLY.contains(&t) {
            continue;
        }
        if let Some((_, sql, want, same)) = IMPORTED.iter().find(|(name, ..)| *name == t) {
            let got = count(&db, sql).await;
            assert_eq!(got, *want, "{table}: {sql}");
            if *same {
                assert_eq!(got, n, "{table} against its Mealie row count");
            }
        } else if let Some((_, part)) = RECIPE_PARTS.iter().find(|(name, _)| *name == t) {
            match t {
                // notifiers and webhooks are webhooks, not recipe parts
                "webhook_urls" => assert_eq!(count(&db, "SELECT count(*) FROM webhooks WHERE kind = 'scheduled'").await, n),
                "group_events_notifiers" | "group_events_notifier_options" => assert_eq!(count(&db, "SELECT count(*) FROM webhooks WHERE kind <> 'scheduled'").await, 2, "{n} rows, one is skipped (discord)"),
                // every recipe has a nutrition row; only a (and not the empty ones of b and c) has any values
                "recipe_nutrition" => assert_eq!((n, recipes.iter().map(part).sum::<usize>()), (3, 1)),
                _ => assert_eq!(recipes.iter().map(part).sum::<usize>() as i64, n, "{table}"),
            }
        } else {
            panic!("table {table} ({n} rows) is neither imported nor skipped on purpose");
        }
    }
    // the report names the tables it only reads, when they have rows
    assert_eq!((rows("recipe_note_ref_link"), skipped(&report, "note references")), (1, 1));
    assert_eq!((rows("shopping_list_recipe_reference"), skipped(&report, "own recipe references")), (1, 1));
}

/// The fixture with `edit` applied to its database.json.
fn mutated(edit: impl Fn(&mut serde_json::Value)) -> (tempfile::TempDir, PathBuf) {
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
            let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            edit(&mut v);
            bytes = serde_json::to_vec(&v).unwrap();
        }
        out.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        out.write_all(&bytes).unwrap();
    }
    out.finish().unwrap();
    (dir, path)
}

#[tokio::test]
async fn a_table_the_importer_does_not_know_is_reported_and_the_skipped_ones_get_one_line_each() {
    use serde_json::json;
    let (_keep, zip) = mutated(|v| {
        v["mystery_table"] = json!([{"id": 1, "secret": "s3cret-value"}, {"id": 2}]);
        v["empty_mystery"] = json!([]);
        v["long_live_tokens"] = json!([{"id": 1, "name": "ci", "token": "tok-s3cret"}, {"id": 2, "name": "x", "token": "tok-other"}]);
        v["ai_providers"] = json!([{"id": "p", "api_key": "sk-s3cret"}]);
        v["server_tasks"] = json!([]);
    });
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, &zip, dir.path()).await.unwrap();
    let text = format!("{report:?}");
    assert_eq!(skipped(&report, "unknown table"), 1, "{:?}", report.skipped);
    assert!(report.skipped.iter().any(|s| s.contains("mystery_table") && s.contains("2 rows")), "{:?}", report.skipped);
    assert!(!text.contains("empty_mystery"), "an empty unknown table says nothing");
    assert!(report.skipped.iter().any(|s| s.contains("long_live_tokens") && s.contains("2 rows")), "{:?}", report.skipped);
    assert!(report.skipped.iter().any(|s| s.contains("ai_providers") && s.contains("1 row")), "{:?}", report.skipped);
    assert!(!text.contains("server_tasks"), "no rows, no line");
    for secret in ["s3cret", "tok-other", "sk-"] {
        assert!(!text.contains(secret), "the report must not carry values: {secret}");
    }
}
