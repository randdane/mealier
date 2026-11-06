//! S35: every store path is scoped to the caller's group or household; S37 (store half): groups,
//! households, the default household and moving users.
mod common;
use mealier::{BatchInput, BatchLineInput, Db, Dimension, EntryInput, Error, IngredientInput, ItemInput, OidcIdentity, RecipeInput, Scope};

macro_rules! not_found {
    ($e:expr) => {
        match $e.await {
            Err(Error::NotFound(_)) => {}
            other => panic!("{}: expected NotFound, got {:?}", stringify!($e), other.map(|_| "Ok")),
        }
    };
}

fn named(name: &str) -> RecipeInput {
    RecipeInput { name: name.into(), ..Default::default() }
}

fn png() -> Vec<u8> {
    let mut b = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    b.extend_from_slice(&[0; 16]);
    b
}

// ---------- S35: recipes, units, foods ----------

#[tokio::test]
async fn recipes_belong_to_a_group_and_are_invisible_to_another() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let r = db.create_recipe(&t.a, named("Soup")).await.unwrap();

    for sc in [&t.a, &t.b] {
        assert_eq!(db.list_recipes(sc, true, None).await.unwrap().len(), 1, "households of one group share recipes");
        assert!(db.get_recipe(sc, r.id).await.unwrap().is_some());
    }
    assert!(db.list_recipes(&t.c, true, None).await.unwrap().is_empty());
    assert!(db.list_recipes(&t.c, true, Some("Soup")).await.unwrap().is_empty(), "search too");
    assert!(db.get_recipe(&t.c, r.id).await.unwrap().is_none());

    not_found!(db.update_recipe(&t.c, r.id, named("Hacked")));
    not_found!(db.archive_recipe(&t.c, r.id));
    not_found!(db.snapshots(&t.c, r.id));
    not_found!(db.snapshot(&t.c, r.current_snapshot_id));
    not_found!(db.recipe_images(&t.c, r.id));
    not_found!(db.comments(&t.c, r.id));
    not_found!(db.list_batches(&t.c, r.id));

    let now = db.get_recipe(&t.a, r.id).await.unwrap().unwrap();
    assert_eq!((now.content.name.as_str(), now.archived), ("Soup", false), "nothing the outsider tried changed it");
    assert_eq!(db.snapshots(&t.a, r.id).await.unwrap().len(), 1, "and no snapshot was written");
    assert_eq!(db.snapshot(&t.b, r.current_snapshot_id).await.unwrap().content.name, "Soup");
}

#[tokio::test]
async fn units_and_foods_belong_to_a_group_and_names_are_unique_per_group() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let g = db.add_unit(&t.a, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    let salt = db.add_food(&t.a, "salt").await.unwrap();

    assert_eq!(db.units(&t.a).await.unwrap().len(), 1);
    assert_eq!(db.units(&t.b).await.unwrap().len(), 1);
    assert!(db.units(&t.c).await.unwrap().is_empty());
    assert!(db.foods(&t.c).await.unwrap().is_empty());
    not_found!(db.rename_unit(&t.c, g, "kilo"));
    not_found!(db.rename_food(&t.c, salt, "pepper"));
    assert_eq!(db.units(&t.a).await.unwrap()[0].name, "g");
    assert_eq!(db.foods(&t.a).await.unwrap()[0].name, "salt");

    assert!(db.add_unit(&t.a, "g", Dimension::Mass, Some(1.0)).await.is_err(), "same group, same name");
    assert!(db.add_food(&t.b, "salt").await.is_err(), "households of one group share the vocabulary");
    db.add_unit(&t.c, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    db.add_food(&t.c, "salt").await.unwrap();
    assert_eq!(db.units(&t.c).await.unwrap().len(), 1, "another group may reuse the names");
    assert_eq!(db.units(&t.a).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_recipe_cannot_use_another_groups_unit_or_food() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let unit_c = db.add_unit(&t.c, "cup", Dimension::Volume, Some(236.0)).await.unwrap();
    let food_c = db.add_food(&t.c, "flour").await.unwrap();
    let with = |unit_id, food_id| RecipeInput { name: "X".into(), ingredients: vec![IngredientInput { unit_id, food_id, ..Default::default() }], ..Default::default() };
    not_found!(db.create_recipe(&t.a, with(Some(unit_c), None)));
    not_found!(db.create_recipe(&t.a, with(None, Some(food_c))));
    let r = db.create_recipe(&t.a, named("Plain")).await.unwrap();
    not_found!(db.update_recipe(&t.a, r.id, with(Some(unit_c), Some(food_c))));
    assert!(db.create_recipe(&t.c, with(Some(unit_c), Some(food_c))).await.is_ok(), "control: its own group may");
    assert_eq!(db.list_recipes(&t.a, true, None).await.unwrap().len(), 1, "the refused create left nothing behind");
}

// ---------- S35: batches ----------

/// A recipe in group One with a unit and a food, and a planned batch of it in household Alpha.
async fn pancakes(db: &Db, t: &common::Tenants) -> (mealier::Recipe, i64, i64, i64) {
    let g = db.add_unit(&t.a, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    let flour = db.add_food(&t.a, "flour").await.unwrap();
    let r = db
        .create_recipe(&t.a, RecipeInput { name: "Pancakes".into(), ingredients: vec![IngredientInput { qty_min: Some(250.0), qty_max: Some(250.0), unit_id: Some(g), food_id: Some(flour), ..Default::default() }], ..Default::default() })
        .await
        .unwrap();
    let b = db.start_batch(&t.a, r.id, 1.0).await.unwrap().batch.id;
    (r, g, flour, b)
}

#[tokio::test]
async fn batches_belong_to_a_household_even_within_one_group() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let (r, _, _, b) = pancakes(&db, &t).await;
    let dir = tempfile::tempdir().unwrap();

    assert_eq!(db.get_batch(&t.a, b).await.unwrap().batch.id, b);
    assert_eq!(db.list_batches(&t.a, r.id).await.unwrap().len(), 1);
    for sc in [&t.b, &t.c] {
        assert!(db.list_batches(sc, r.id).await.map(|v| v.is_empty()).unwrap_or(true), "no batches listed");
        not_found!(db.get_batch(sc, b));
        not_found!(db.update_batch(sc, b, BatchInput { scale: 1.0, notes: "hacked".into(), ..Default::default() }));
        not_found!(db.add_batch_line(sc, b, BatchLineInput::default()));
        not_found!(db.finish_batch(sc, b));
        not_found!(db.add_batch_photo(sc, b, dir.path(), &png()));
    }
    // household Beta, same group: the recipe is shared, the batch listing is not.
    assert!(db.list_batches(&t.b, r.id).await.unwrap().is_empty());
    not_found!(db.list_batches(&t.c, r.id));

    let now = db.get_batch(&t.a, b).await.unwrap();
    assert_eq!((now.batch.status.as_str(), now.batch.notes.as_str(), now.lines.len(), now.photos.len()), ("planned", "", 1, 0), "untouched");
}

#[tokio::test]
async fn a_batch_needs_a_recipe_in_the_households_group() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let (r, ..) = pancakes(&db, &t).await;
    not_found!(db.start_batch(&t.c, r.id, 1.0));
    let mine = db.start_batch(&t.b, r.id, 2.0).await.unwrap();
    assert_eq!(mine.batch.recipe_id, r.id, "control: Beta shares the group's recipe");
    assert!(db.get_batch(&t.b, mine.batch.id).await.is_ok());
    not_found!(db.get_batch(&t.a, mine.batch.id));
}

#[tokio::test]
async fn batch_lines_cannot_point_at_another_groups_units_or_foods() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let (_, _, _, b) = pancakes(&db, &t).await;
    let unit_c = db.add_unit(&t.c, "cup", Dimension::Volume, Some(236.0)).await.unwrap();
    let food_c = db.add_food(&t.c, "milk").await.unwrap();
    let food_a = db.add_food(&t.a, "milk").await.unwrap();

    not_found!(db.add_batch_line(&t.a, b, BatchLineInput { unit_id: Some(unit_c), ..Default::default() }));
    not_found!(db.add_batch_line(&t.a, b, BatchLineInput { food_id: Some(food_c), ..Default::default() }));
    not_found!(db.add_batch_line(&t.a, b, BatchLineInput { substitute_food_id: Some(food_c), ..Default::default() }));
    let detail = db.get_batch(&t.a, b).await.unwrap();
    let mut lines: Vec<BatchLineInput> = detail.lines.iter().map(|l| BatchLineInput { id: Some(l.id), qty_min: l.qty_min, qty_max: l.qty_max, unit_id: l.unit_id, ..Default::default() }).collect();
    lines[0].substitute_food_id = Some(food_c);
    not_found!(db.update_batch(&t.a, b, BatchInput { scale: 1.0, lines, ..Default::default() }));
    assert_eq!(db.get_batch(&t.a, b).await.unwrap().lines.len(), 1, "nothing was added");
    db.add_batch_line(&t.a, b, BatchLineInput { food_id: Some(food_a), ..Default::default() }).await.unwrap();
}

// ---------- S35: meal plan ----------

#[tokio::test]
async fn the_meal_plan_belongs_to_a_household() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let (r, _, _, batch) = pancakes(&db, &t).await;
    let entry = |recipe_id, batch_id| EntryInput { date: "2026-12-01".into(), slot: "dinner".into(), recipe_id, batch_id, ..Default::default() };
    let e = db.add_entry(&t.a, entry(Some(r.id), Some(batch))).await.unwrap();

    assert_eq!(db.meal_plan(&t.a, "2026-12-01", "2026-12-31").await.unwrap().len(), 1);
    for sc in [&t.b, &t.c] {
        assert!(db.meal_plan(sc, "2026-12-01", "2026-12-31").await.unwrap().is_empty());
        not_found!(db.update_entry(sc, e.id, EntryInput { title: "hacked".into(), ..entry(None, None) }));
        not_found!(db.delete_entry(sc, e.id));
    }
    let after = db.meal_plan(&t.a, "2026-12-01", "2026-12-31").await.unwrap();
    assert_eq!((after.len(), after[0].recipe_id, after[0].batch_id), (1, Some(r.id), Some(batch)), "still there, unchanged");

    // what an entry may point at
    not_found!(db.add_entry(&t.c, entry(Some(r.id), None)));
    not_found!(db.add_entry(&t.b, entry(Some(r.id), Some(batch))));
    db.add_entry(&t.b, entry(Some(r.id), None)).await.unwrap();
    let other = db.create_recipe(&t.c, named("Elsewhere")).await.unwrap();
    not_found!(db.add_entry(&t.a, entry(Some(other.id), None)));
    not_found!(db.update_entry(&t.a, e.id, entry(Some(other.id), None)));
    assert_eq!(db.meal_plan(&t.b, "2026-12-01", "2026-12-31").await.unwrap().len(), 1, "Beta has only its own entry");
}

// ---------- S35: shopping lists ----------

#[tokio::test]
async fn shopping_lists_belong_to_a_household() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let (r, g, flour, batch) = pancakes(&db, &t).await;
    let list = db.create_list(&t.a, "Weekly").await.unwrap();
    let item = db.add_item(&t.a, list.id, ItemInput { qty: Some(1.0), unit_id: Some(g), food_id: Some(flour), ..Default::default() }).await.unwrap();

    assert_eq!(db.shopping_lists(&t.a).await.unwrap().len(), 1);
    for sc in [&t.b, &t.c] {
        assert!(db.shopping_lists(sc).await.unwrap().is_empty());
        not_found!(db.shopping_list(sc, list.id));
        not_found!(db.add_item(sc, list.id, ItemInput { note: "x".into(), ..Default::default() }));
        not_found!(db.update_item(sc, item.id, ItemInput { note: "x".into(), ..Default::default() }));
        not_found!(db.set_item_checked(sc, item.id, true));
        not_found!(db.delete_item(sc, item.id));
        not_found!(db.add_recipe_to_list(sc, list.id, r.id, 1.0));
        not_found!(db.add_batch_to_list(sc, list.id, batch));
    }
    let now = db.shopping_list(&t.a, list.id).await.unwrap();
    assert_eq!((now.items.len(), now.items[0].checked, now.items[0].note.as_str()), (1, false, ""), "untouched");
}

#[tokio::test]
async fn shopping_items_cannot_use_things_from_outside_the_scope() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let (r, _, _, batch) = pancakes(&db, &t).await;
    let unit_c = db.add_unit(&t.c, "cup", Dimension::Volume, Some(236.0)).await.unwrap();
    let food_c = db.add_food(&t.c, "milk").await.unwrap();
    let mine = db.create_list(&t.a, "Mine").await.unwrap();
    let beta = db.create_list(&t.b, "Beta's").await.unwrap();
    let theirs = db.create_recipe(&t.c, named("Theirs")).await.unwrap();

    not_found!(db.add_item(&t.a, mine.id, ItemInput { qty: Some(1.0), unit_id: Some(unit_c), note: "x".into(), ..Default::default() }));
    not_found!(db.add_item(&t.a, mine.id, ItemInput { food_id: Some(food_c), ..Default::default() }));
    not_found!(db.add_recipe_to_list(&t.a, mine.id, theirs.id, 1.0));
    // Beta shares the recipe (group) but not Alpha's batch (household)
    db.add_recipe_to_list(&t.b, beta.id, r.id, 1.0).await.unwrap();
    not_found!(db.add_batch_to_list(&t.b, beta.id, batch));
    db.add_batch_to_list(&t.a, mine.id, batch).await.unwrap();
    assert_eq!(db.shopping_list(&t.a, mine.id).await.unwrap().items.len(), 1);
    assert!(db.shopping_list(&t.b, beta.id).await.unwrap().items.iter().all(|i| i.recipe_id == Some(r.id)));
}

// ---------- S37 (store): groups, households, default household, moving users ----------

#[tokio::test]
async fn home_in_home_is_made_when_there_is_no_default_household() {
    let db = Db::open_memory().await.unwrap();
    assert!(db.groups().await.unwrap().is_empty());
    let h = db.default_household().await.unwrap();
    assert_eq!((h.name.as_str(), h.slug.as_str()), ("Home", "home"));
    let groups = db.groups().await.unwrap();
    assert_eq!(groups.iter().map(|g| (g.name.as_str(), g.slug.as_str(), g.id == h.group_id)).collect::<Vec<_>>(), vec![("Home", "home", true)]);
    assert_eq!(db.default_household().await.unwrap(), h, "made once");
    assert_eq!(db.households().await.unwrap(), vec![h]);
}

#[tokio::test]
async fn groups_and_households_get_unique_slugs_and_validated_names() {
    let db = Db::open_memory().await.unwrap();
    let a = db.create_group("Family Kitchen").await.unwrap();
    let b = db.create_group("Family  kitchen!").await.unwrap();
    assert_eq!((a.slug.as_str(), b.slug.as_str()), ("family-kitchen", "family-kitchen-2"));
    assert!(matches!(db.create_group("   ").await, Err(Error::Validation(_))));
    assert!(matches!(db.create_household(a.id, "").await, Err(Error::Validation(_))));
    not_found!(db.create_household(999, "Nowhere"));

    let h1 = db.create_household(a.id, "Upstairs").await.unwrap();
    let h2 = db.create_household(a.id, "upstairs").await.unwrap();
    let h3 = db.create_household(b.id, "Upstairs").await.unwrap();
    assert_eq!((h1.slug.as_str(), h2.slug.as_str(), h3.slug.as_str()), ("upstairs", "upstairs-2", "upstairs"), "slugs are unique per group");
    assert_eq!((h1.group_id, h3.group_id), (a.id, b.id));
    assert_eq!(db.groups().await.unwrap().len(), 2);
    assert_eq!(db.households().await.unwrap().len(), 3);
    assert_eq!(db.households().await.unwrap()[0], h1);
}

#[tokio::test]
async fn the_default_household_can_be_chosen() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    not_found!(db.set_default_household(999));
    let home = db.default_household().await.unwrap();
    assert_eq!(home.name, "Home", "nothing chosen yet");
    let set = db.set_default_household(t.c.household_id).await.unwrap();
    assert_eq!(set.id, t.c.household_id);
    assert_eq!(db.default_household().await.unwrap().id, t.c.household_id);
}

fn identity(n: &str) -> OidcIdentity {
    OidcIdentity { issuer: "https://idp.test".into(), subject: format!("sub-{n}"), email: format!("{n}@example.test"), name: n.into(), username: n.into(), admin: false }
}

#[tokio::test]
async fn oidc_and_local_admin_users_join_the_default_household() {
    let db = Db::open_memory().await.unwrap();
    let first = db.oidc_login(&identity("ann")).await.unwrap();
    let home = db.default_household().await.unwrap();
    assert_eq!((first.household_id, first.group_id), (home.id, home.group_id), "Home/Home was made for the first user");
    assert_eq!(db.groups().await.unwrap().len(), 1);

    let t = common::tenants(&db).await;
    db.set_default_household(t.c.household_id).await.unwrap();
    let second = db.oidc_login(&identity("bob")).await.unwrap();
    assert_eq!((second.household_id, second.group_id), (t.c.household_id, t.c.group_id));
    let again = db.oidc_login(&identity("ann")).await.unwrap();
    assert_eq!(again.household_id, home.id, "an existing user stays where they are");

    let admin = db.local_admin().await.unwrap();
    assert_eq!((admin.household_id, admin.group_id), (t.c.household_id, t.c.group_id));
    assert_eq!(db.local_admin().await.unwrap().household_id, t.c.household_id);

    let fresh = Db::open_memory().await.unwrap();
    let admin = fresh.local_admin().await.unwrap();
    let home = fresh.default_household().await.unwrap();
    assert_eq!((admin.household_id, home.name.as_str()), (home.id, "Home"), "the local admin also gets Home/Home when nothing exists");
}

#[tokio::test]
async fn a_scope_is_derived_from_the_users_row() {
    let db = Db::open_memory().await.unwrap();
    let mut ident = identity("ann");
    ident.admin = true;
    let u = db.oidc_login(&ident).await.unwrap();
    let home = db.default_household().await.unwrap();
    assert_eq!(Scope::from(&u), Scope { user_id: u.id, group_id: home.group_id, household_id: home.id, admin: true });
    assert_eq!(Scope::from(&db.user(u.id).await.unwrap()), Scope::from(&u));
    let found = db.session_user(&db.create_session(u.id).await.unwrap()).await.unwrap().unwrap();
    assert_eq!((found.household_id, found.group_id), (home.id, home.group_id));
}

#[tokio::test]
async fn moving_a_user_changes_their_tenant_but_not_the_data_they_leave_behind() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    db.set_default_household(t.a.household_id).await.unwrap();
    let ann = db.oidc_login(&identity("ann")).await.unwrap();
    assert_eq!(Scope::from(&ann).household_id, t.a.household_id);
    let (r, _, _, batch) = pancakes(&db, &t).await;
    let elsewhere = db.create_recipe(&t.c, named("Gamma soup")).await.unwrap();

    not_found!(db.move_user(ann.id, 999));
    not_found!(db.move_user(999, t.c.household_id));
    let moved = db.move_user(ann.id, t.c.household_id).await.unwrap();
    assert_eq!((moved.household_id, moved.group_id), (t.c.household_id, t.c.group_id), "across groups is allowed");
    let sc = Scope::from(&db.user(ann.id).await.unwrap());
    assert_eq!((sc.group_id, sc.household_id), (t.c.group_id, t.c.household_id));

    not_found!(db.get_batch(&sc, batch));
    assert!(db.get_recipe(&sc, r.id).await.unwrap().is_none());
    assert!(db.get_recipe(&sc, elsewhere.id).await.unwrap().is_some());
    assert_eq!(db.get_batch(&t.a, batch).await.unwrap().batch.id, batch, "the batch stayed with household Alpha");

    let back = db.move_user(ann.id, t.b.household_id).await.unwrap();
    assert_eq!((back.household_id, back.group_id), (t.b.household_id, t.b.group_id));
}
