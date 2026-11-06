//! Step 14, part A, at the store: ingredient sections and stable line ids, step titles and links,
//! substitutions, notes, extras, settings (the lock), assets, comments, food substitutions and
//! extras on foods and shopping lists. Everything that describes a recipe is frozen in its snapshots.
mod common;
use std::collections::BTreeMap;

use mealier::{Db, Error, IngredientInput, LineDetail, Note, RecipeInput, Settings, StepDetail, SubstitutionInput, Substitution, clean_file_name};

fn line(t: &str) -> IngredientInput {
    IngredientInput { original_text: t.into(), ..Default::default() }
}

fn detail(section: &str) -> LineDetail {
    LineDetail { section: section.into(), ..Default::default() }
}

fn validation<T: std::fmt::Debug>(r: Result<T, Error>, needle: &str) {
    match r {
        Err(Error::Validation(m)) => assert!(m.contains(needle), "{m:?} should mention {needle:?}"),
        other => panic!("expected a validation error mentioning {needle:?}, got {other:?}"),
    }
}

fn not_found<T: std::fmt::Debug>(r: Result<T, Error>) {
    assert!(matches!(r, Err(Error::NotFound(_))), "expected NotFound, got {r:?}");
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

// ---------- ingredient lines: uid, sections, substitutions ----------

#[tokio::test]
async fn every_ingredient_line_gets_its_own_id_and_keeps_it_through_an_edit_that_leaves_it_alone() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ingredients: vec![line("2 onions"), line("1 tbsp oil")], ..Default::default() }).await.unwrap();
    let (a, b) = (r.content.ingredients[0].uid.clone(), r.content.ingredients[1].uid.clone());
    assert!(!a.is_empty() && !b.is_empty() && a != b, "{a:?} {b:?}");

    // an edit that sends the ids back and adds a line in front
    let input = RecipeInput {
        name: "Soup".into(),
        ingredients: vec![line("salt"), line("2 onions"), line("1 tbsp oil")],
        ingredient_details: vec![detail(""), LineDetail { uid: Some(a.clone()), ..Default::default() }, LineDetail { uid: Some(b.clone()), ..Default::default() }],
        ..Default::default()
    };
    let r2 = db.update_recipe(&sc, r.id, input).await.unwrap();
    let uids: Vec<_> = r2.content.ingredients.iter().map(|i| i.uid.as_str()).collect();
    assert_eq!(uids[1..], [a.as_str(), b.as_str()]);
    assert!(!uids[0].is_empty() && uids[0] != a && uids[0] != b, "the new line got a new id");
    // and the first snapshot still has what it had
    let first = db.snapshot(&sc, r.current_snapshot_id).await.unwrap().content;
    assert_eq!(first.ingredients.len(), 2);
    assert_eq!(first.ingredients[0].uid, a);
}

#[tokio::test]
async fn a_line_id_must_be_plain_and_not_repeated() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let with = |ids: [&str; 2]| RecipeInput {
        name: "Soup".into(),
        ingredients: vec![line("a"), line("b")],
        ingredient_details: ids.iter().map(|u| LineDetail { uid: Some(u.to_string()), ..Default::default() }).collect(),
        ..Default::default()
    };
    validation(db.create_recipe(&sc, with(["x", "x"])).await, "twice");
    validation(db.create_recipe(&sc, with(["a b", "c"])).await, "line id");
    validation(db.create_recipe(&sc, with(["a<b", "c"])).await, "line id");
    let ok = db.create_recipe(&sc, with(["abc_1-2", "Z9"])).await.unwrap();
    assert_eq!(ok.content.ingredients[0].uid, "abc_1-2");
}

#[tokio::test]
async fn details_for_more_lines_than_there_are_are_refused() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let input = RecipeInput { name: "Soup".into(), ingredients: vec![line("a")], ingredient_details: vec![detail(""), detail("")], ..Default::default() };
    validation(db.create_recipe(&sc, input).await, "ingredient_details");
}

#[tokio::test]
async fn a_line_can_start_a_section_and_carry_substitutions() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let sub = Substitution { qty: Some(1.5), unit: Some("tbsp".into()), food: Some("butter".into()), note: "if no oil".into() };
    let input = RecipeInput {
        name: "Soup".into(),
        ingredients: vec![line("2 onions"), line("1 tbsp oil")],
        ingredient_details: vec![detail("Base"), LineDetail { section: "Sauce".into(), substitutions: vec![sub.clone()], ..Default::default() }],
        ..Default::default()
    };
    let r = db.create_recipe(&sc, input).await.unwrap();
    assert_eq!(r.content.ingredients[0].section, "Base");
    assert_eq!((r.content.ingredients[1].section.as_str(), r.content.ingredients[1].substitutions.clone()), ("Sauce", vec![sub.clone()]));
    let got = db.get_recipe(&sc, r.id).await.unwrap().unwrap();
    assert_eq!(got.content.ingredients[1].substitutions, vec![sub]);
}

#[tokio::test]
async fn a_substitution_needs_a_quantity_that_is_a_sane_number() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let with = |qty| RecipeInput {
        name: "Soup".into(),
        ingredients: vec![line("a")],
        ingredient_details: vec![LineDetail { substitutions: vec![Substitution { qty: Some(qty), food: Some("x".into()), ..Default::default() }], ..Default::default() }],
        ..Default::default()
    };
    validation(db.create_recipe(&sc, with(-1.0)).await, "quantit");
    validation(db.create_recipe(&sc, with(f64::NAN)).await, "quantit");
    assert!(db.create_recipe(&sc, with(0.5)).await.is_ok());
}

// ---------- steps ----------

#[tokio::test]
async fn a_step_has_an_optional_title_and_links_to_ingredient_lines_by_id() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ingredients: vec![line("2 onions"), line("oil")], steps: vec!["Chop".into(), "Fry".into()], ..Default::default() }).await.unwrap();
    let (a, b) = (r.content.ingredients[0].uid.clone(), r.content.ingredients[1].uid.clone());
    assert_eq!(r.content.step_details.len(), 2, "one entry per step, empty when nothing was said");
    assert_eq!(r.content.step_details[0], StepDetail::default());

    // an edit that keeps the ids and links the steps
    let input = RecipeInput {
        name: "Soup".into(),
        ingredients: vec![line("2 onions"), line("oil")],
        ingredient_details: vec![LineDetail { uid: Some(a.clone()), ..Default::default() }, LineDetail { uid: Some(b.clone()), ..Default::default() }],
        steps: vec!["Chop".into(), "Fry".into()],
        step_details: vec![StepDetail { title: "Prep".into(), ingredient_uids: vec![a.clone()] }, StepDetail { title: String::new(), ingredient_uids: vec![a.clone(), b.clone()] }],
        ..Default::default()
    };
    let r2 = db.update_recipe(&sc, r.id, input).await.unwrap();
    assert_eq!(r2.content.step_details[0], StepDetail { title: "Prep".into(), ingredient_uids: vec![a.clone()] });
    assert_eq!(r2.content.step_details[1].ingredient_uids, vec![a, b]);
    // the old snapshot did not change
    assert_eq!(db.snapshot(&sc, r.current_snapshot_id).await.unwrap().content.step_details[0], StepDetail::default());
}

#[tokio::test]
async fn a_step_cannot_link_to_a_line_the_recipe_does_not_have_and_details_cannot_outnumber_steps() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let base = || RecipeInput { name: "Soup".into(), ingredients: vec![line("a")], steps: vec!["Stir".into()], ..Default::default() };
    let bad_link = RecipeInput { step_details: vec![StepDetail { title: String::new(), ingredient_uids: vec!["nope".into()] }], ..base() };
    validation(db.create_recipe(&sc, bad_link).await, "unknown ingredient");
    let too_many = RecipeInput { step_details: vec![StepDetail::default(), StepDetail::default()], ..base() };
    validation(db.create_recipe(&sc, too_many).await, "step_details");
}

// ---------- notes, extras, settings ----------

#[tokio::test]
async fn notes_extras_and_settings_are_saved_replaced_and_kept_when_left_out() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let settings = Settings { hide_nutrition: true, public: true, landscape_view: true, show_assets: true, disable_amount: true, ..Default::default() };
    let r = db
        .create_recipe(
            &sc,
            RecipeInput {
                name: "Soup".into(),
                notes: Some(vec![Note { title: "Tip".into(), text: "Salt late".into() }]),
                extras: Some(map(&[("source", "gran"), ("k2", "v2")])),
                settings: Some(settings.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.content.notes, vec![Note { title: "Tip".into(), text: "Salt late".into() }]);
    assert_eq!(r.content.extras, map(&[("k2", "v2"), ("source", "gran")]));
    assert_eq!(r.content.settings, settings);

    // left out: kept
    let kept = db.update_recipe(&sc, r.id, RecipeInput { name: "Soup 2".into(), ..Default::default() }).await.unwrap();
    assert_eq!((kept.content.notes.len(), kept.content.extras.len(), kept.content.settings.clone()), (1, 2, settings));
    // sent empty: replaced
    let cleared = db.update_recipe(&sc, r.id, RecipeInput { name: "Soup 2".into(), notes: Some(vec![]), extras: Some(BTreeMap::new()), ..Default::default() }).await.unwrap();
    assert_eq!((cleared.content.notes.len(), cleared.content.extras.len()), (0, 0));
    // frozen: the first snapshot still has them
    let first = db.snapshot(&sc, r.current_snapshot_id).await.unwrap().content;
    assert_eq!((first.notes.len(), first.extras.len()), (1, 2));
}

#[tokio::test]
async fn extras_keys_must_not_be_blank_and_notes_need_some_text() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    validation(db.create_recipe(&sc, RecipeInput { name: "x".into(), extras: Some(map(&[(" ", "v")])), ..Default::default() }).await, "extras");
    validation(db.create_recipe(&sc, RecipeInput { name: "x".into(), notes: Some(vec![Note { title: "t".into(), text: " ".into() }]), ..Default::default() }).await, "note");
}

#[tokio::test]
async fn a_locked_recipe_refuses_edits_until_a_request_unlocks_it() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), settings: Some(Settings { locked: true, ..Default::default() }), ..Default::default() }).await.unwrap();
    assert!(r.content.settings.locked);

    validation(db.update_recipe(&sc, r.id, RecipeInput { name: "Edited".into(), ..Default::default() }).await, "recipe is locked");
    validation(db.update_recipe(&sc, r.id, RecipeInput { name: "Edited".into(), settings: Some(Settings { locked: true, ..Default::default() }), ..Default::default() }).await, "recipe is locked");
    validation(db.set_recipe_image(&sc, r.id, &tempfile::tempdir().unwrap().keep(), &common::png(4, 4, 1)).await, "recipe is locked");
    assert_eq!(db.get_recipe(&sc, r.id).await.unwrap().unwrap().content.name, "Soup");
    assert_eq!(db.snapshots(&sc, r.id).await.unwrap().len(), 1, "refused edits write nothing");

    let open = db.update_recipe(&sc, r.id, RecipeInput { name: "Edited".into(), settings: Some(Settings::default()), ..Default::default() }).await.unwrap();
    assert_eq!((open.content.name.as_str(), open.content.settings.locked), ("Edited", false));
    assert!(db.update_recipe(&sc, r.id, RecipeInput { name: "Again".into(), ..Default::default() }).await.is_ok());
}

#[tokio::test]
async fn locking_is_an_edit_that_an_unlocked_recipe_allows_and_archiving_ignores_the_lock() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    db.update_recipe(&sc, r.id, RecipeInput { name: "Soup".into(), settings: Some(Settings { locked: true, ..Default::default() }), ..Default::default() }).await.unwrap();
    assert!(db.get_recipe(&sc, r.id).await.unwrap().unwrap().content.settings.locked);
    validation(db.update_recipe(&sc, r.id, RecipeInput { name: "Edited".into(), ..Default::default() }).await, "recipe is locked");
    db.archive_recipe(&sc, r.id).await.unwrap();
    assert!(db.get_recipe(&sc, r.id).await.unwrap().unwrap().archived);
}

#[tokio::test]
async fn comments_are_refused_when_the_recipe_disables_them() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let open = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let shut = db.create_recipe(&sc, RecipeInput { name: "Stew".into(), settings: Some(Settings { disable_comments: true, ..Default::default() }), ..Default::default() }).await.unwrap();
    let c = db.add_comment(&sc, open.id, "  lovely  ").await.unwrap();
    assert_eq!(c.text, "lovely");
    assert_eq!(db.comments(&sc, open.id).await.unwrap().len(), 1);
    validation(db.add_comment(&sc, shut.id, "hi").await, "comments are disabled");
    assert!(db.comments(&sc, shut.id).await.unwrap().is_empty());
    validation(db.add_comment(&sc, open.id, "  ").await, "comment");
}

// ---------- assets ----------

#[test]
fn download_names_are_cleaned() {
    for (raw, want) in [
        ("menu.pdf", "menu.pdf"),
        ("../../etc/passwd", "passwd"),
        ("C:\\Users\\me\\Plan (1).docx", "Plan (1).docx"),
        (".htaccess", "htaccess"),
        ("a\"b;c\r\nd.txt", "a_b_c__d.txt"),
        ("crème brûlée.pdf", "cr_me br_l_e.pdf"),
        ("", "file"),
        ("////", "file"),
        ("...", "file"),
    ] {
        assert_eq!(clean_file_name(raw), want, "{raw:?}");
    }
    let long = format!("{}.pdf", "a".repeat(300));
    assert_eq!(clean_file_name(&long).chars().count(), 100);
}

#[tokio::test]
async fn an_asset_is_stored_as_given_listed_in_the_snapshot_and_kept_through_later_edits() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    // not an image, and looks like a script: stored untouched, never decoded
    let bytes = b"<script>alert(1)</script>\x00\xff not an image".to_vec();
    let a = db.add_recipe_asset(&sc, r.id, dir.path(), "../evil.html", "Menu", "mdi-file", &bytes).await.unwrap();
    assert_eq!((a.name.as_str(), a.icon.as_str(), a.file_name.as_str(), a.size), ("Menu", "mdi-file", "evil.html", bytes.len() as i64));
    assert_eq!(a.hash.len(), 64);
    assert_eq!(std::fs::read(dir.path().join(&a.hash)).unwrap(), bytes);

    let now = db.get_recipe(&sc, r.id).await.unwrap().unwrap();
    assert_eq!(now.content.assets, vec![a.hash.clone()]);
    assert_ne!(now.current_snapshot_id, r.current_snapshot_id, "attaching is an edit");
    assert!(db.snapshot(&sc, r.current_snapshot_id).await.unwrap().content.assets.is_empty());
    assert_eq!(db.recipe_assets(&sc, r.id).await.unwrap(), vec![a.clone()]);
    assert_eq!(db.recipe_asset(&sc, r.id, &a.hash).await.unwrap(), a);

    // a later edit keeps it in the new snapshot
    let edited = db.update_recipe(&sc, r.id, RecipeInput { name: "Soup 2".into(), ..Default::default() }).await.unwrap();
    assert_eq!(edited.content.assets, vec![a.hash.clone()]);
    // the same file again changes nothing
    let again = db.add_recipe_asset(&sc, r.id, dir.path(), "evil.html", "Menu", "mdi-file", &bytes).await.unwrap();
    assert_eq!(again.hash, a.hash);
    assert_eq!(db.recipe_assets(&sc, r.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn assets_take_any_type_but_not_nothing_and_not_more_than_20_mb() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    validation(db.add_recipe_asset(&sc, r.id, dir.path(), "a.bin", "", "", b"").await, "empty");
    let max = vec![7u8; 20 * 1024 * 1024];
    assert!(db.add_recipe_asset(&sc, r.id, dir.path(), "max.bin", "", "", &max).await.is_ok());
    let over = vec![8u8; 20 * 1024 * 1024 + 1];
    validation(db.add_recipe_asset(&sc, r.id, dir.path(), "over.bin", "", "", &over).await, "20 MB");
    assert_eq!(db.recipe_assets(&sc, r.id).await.unwrap().len(), 1);
    let a = &db.recipe_assets(&sc, r.id).await.unwrap()[0];
    assert_eq!(a.name, "max.bin", "a blank name falls back to the file name");
}

#[tokio::test]
async fn removing_an_asset_writes_a_snapshot_without_it_and_a_locked_recipe_refuses_both_ways() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let a = db.add_recipe_asset(&sc, r.id, dir.path(), "a.txt", "A", "", b"hello").await.unwrap();
    let with = db.get_recipe(&sc, r.id).await.unwrap().unwrap();

    db.update_recipe(&sc, r.id, RecipeInput { name: "Soup".into(), settings: Some(Settings { locked: true, ..Default::default() }), ..Default::default() }).await.unwrap();
    validation(db.remove_recipe_asset(&sc, r.id, &a.hash).await, "recipe is locked");
    validation(db.add_recipe_asset(&sc, r.id, dir.path(), "b.txt", "B", "", b"world").await, "recipe is locked");
    assert_eq!(db.recipe_assets(&sc, r.id).await.unwrap().len(), 1, "a refused attach leaves no row");
    db.update_recipe(&sc, r.id, RecipeInput { name: "Soup".into(), settings: Some(Settings::default()), ..Default::default() }).await.unwrap();

    db.remove_recipe_asset(&sc, r.id, &a.hash).await.unwrap();
    assert!(db.get_recipe(&sc, r.id).await.unwrap().unwrap().content.assets.is_empty());
    assert_eq!(db.snapshot(&sc, with.current_snapshot_id).await.unwrap().content.assets, vec![a.hash.clone()], "history still lists it");
    not_found(db.recipe_asset(&sc, r.id, &a.hash).await);
    not_found(db.remove_recipe_asset(&sc, r.id, &a.hash).await);
}

#[tokio::test]
async fn assets_belong_to_the_recipes_group() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let r = db.create_recipe(&t.a, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let a = db.add_recipe_asset(&t.a, r.id, dir.path(), "a.txt", "A", "", b"hello").await.unwrap();
    // a household of the same group shares it; another group sees nothing, even holding the same bytes
    assert!(db.recipe_asset(&t.b, r.id, &a.hash).await.is_ok());
    not_found(db.recipe_asset(&t.c, r.id, &a.hash).await);
    not_found(db.recipe_assets(&t.c, r.id).await);
    not_found(db.add_recipe_asset(&t.c, r.id, dir.path(), "x.txt", "", "", b"x").await);
    not_found(db.remove_recipe_asset(&t.c, r.id, &a.hash).await);
    let theirs = db.create_recipe(&t.c, RecipeInput { name: "Theirs".into(), ..Default::default() }).await.unwrap();
    db.add_recipe_asset(&t.c, theirs.id, dir.path(), "a.txt", "A", "", b"hello").await.unwrap();
    not_found(db.recipe_asset(&t.c, r.id, &a.hash).await);
    not_found(db.recipe_asset(&t.a, theirs.id, &a.hash).await);
}

// ---------- foods: substitutions and extras; lists and items: extras ----------

#[tokio::test]
async fn a_food_lists_the_foods_that_can_stand_in_for_it() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let butter = db.add_food(&sc, "butter").await.unwrap();
    let oil = db.add_food(&sc, "oil").await.unwrap();
    let ghee = db.add_food(&sc, "ghee").await.unwrap();

    let subs = db.add_food_substitution(&sc, butter, SubstitutionInput { food_id: oil, note: "1:1".into() }).await.unwrap();
    assert_eq!((subs[0].food_id, subs[0].name.as_str(), subs[0].note.as_str()), (oil, "oil", "1:1"));
    db.add_food_substitution(&sc, butter, SubstitutionInput { food_id: ghee, note: String::new() }).await.unwrap();
    let f = db.food(&sc, butter).await.unwrap();
    assert_eq!(f.substitutions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["ghee", "oil"], "by name");
    assert!(db.food(&sc, oil).await.unwrap().substitutions.is_empty(), "one way");

    let oil_entry = f.substitutions.iter().find(|s| s.food_id == oil).unwrap().id;
    let upd = db.update_food_substitution(&sc, oil_entry, "3:4").await.unwrap();
    assert_eq!(upd.iter().find(|s| s.food_id == oil).unwrap().note, "3:4");
    db.remove_food_substitution(&sc, oil_entry).await.unwrap();
    assert_eq!(db.food(&sc, butter).await.unwrap().substitutions.len(), 1);
    not_found(db.remove_food_substitution(&sc, oil_entry).await);
    not_found(db.update_food_substitution(&sc, oil_entry, "x").await);

    validation(db.add_food_substitution(&sc, butter, SubstitutionInput { food_id: butter, note: String::new() }).await, "itself");
    let again = db.add_food_substitution(&sc, butter, SubstitutionInput { food_id: ghee, note: "clarified".into() }).await.unwrap();
    assert_eq!(again.iter().map(|s| (s.name.as_str(), s.note.as_str())).collect::<Vec<_>>(), [("ghee", "clarified")], "adding a food again changes its note");
}

#[tokio::test]
async fn food_substitutions_stay_inside_the_group() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let mine = db.add_food(&t.a, "butter").await.unwrap();
    let mine2 = db.add_food(&t.a, "oil").await.unwrap();
    let theirs = db.add_food(&t.c, "butter").await.unwrap();
    not_found(db.add_food_substitution(&t.a, mine, SubstitutionInput { food_id: theirs, note: String::new() }).await);
    not_found(db.add_food_substitution(&t.c, mine, SubstitutionInput { food_id: theirs, note: String::new() }).await);
    not_found(db.food(&t.c, mine).await);
    let entry = db.add_food_substitution(&t.a, mine, SubstitutionInput { food_id: mine2, note: String::new() }).await.unwrap()[0].id;
    // the other household of the group shares them
    assert_eq!(db.food(&t.b, mine).await.unwrap().substitutions.len(), 1);
    not_found(db.remove_food_substitution(&t.c, entry).await);
    not_found(db.update_food_substitution(&t.c, entry, "x").await);
    assert_eq!(db.food(&t.a, mine).await.unwrap().substitutions.len(), 1, "another group's attempts changed nothing");
}

#[tokio::test]
async fn foods_lists_and_items_carry_extras() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let food = db.add_food(&t.a, "butter").await.unwrap();
    db.set_food_extras(&t.a, food, map(&[("aisle", "3")])).await.unwrap();
    assert_eq!(db.food(&t.a, food).await.unwrap().extras, map(&[("aisle", "3")]));
    not_found(db.set_food_extras(&t.c, food, map(&[("x", "y")])).await);
    validation(db.set_food_extras(&t.a, food, map(&[("", "y")])).await, "extras");

    let list = db.create_list(&t.a, "Weekly").await.unwrap();
    assert!(db.shopping_list(&t.a, list.id).await.unwrap().extras.is_empty());
    db.set_list_extras(&t.a, list.id, map(&[("store", "co-op")])).await.unwrap();
    assert_eq!(db.shopping_list(&t.a, list.id).await.unwrap().extras, map(&[("store", "co-op")]));
    not_found(db.set_list_extras(&t.b, list.id, map(&[("x", "y")])).await);

    let item = db.add_item(&t.a, list.id, mealier::ItemInput { note: "milk".into(), extras: Some(map(&[("brand", "x")])), ..Default::default() }).await.unwrap();
    assert_eq!(item.extras, map(&[("brand", "x")]));
    let kept = db.update_item(&t.a, item.id, mealier::ItemInput { note: "milk 2".into(), ..Default::default() }).await.unwrap();
    assert_eq!(kept.extras, map(&[("brand", "x")]), "left out: kept");
    let cleared = db.update_item(&t.a, item.id, mealier::ItemInput { note: "milk 2".into(), extras: Some(BTreeMap::new()), ..Default::default() }).await.unwrap();
    assert!(cleared.extras.is_empty());
    let l = db.shopping_list(&t.a, list.id).await.unwrap();
    assert!(l.items[0].extras.is_empty());
}
