//! Mealie's query_filter_string: the parser (literal strings to AST, error cases) and the evaluator
//! (a fixed recipe set, literal expected names).
mod common;
use mealier::filter::{By, CmpOp, Field, Filter, Lit, SetField, SetOp, TextField, TextOp, parse};

fn ok(src: &str) -> Filter {
    parse(src).unwrap_or_else(|e| panic!("{src}: {e}")).unwrap_or_else(|| panic!("{src}: parsed to nothing"))
}

fn err(src: &str) -> String {
    match parse(src) {
        Ok(f) => panic!("{src} should be refused, got {f:?}"),
        Err(e) => e.to_string(),
    }
}

fn set(field: SetField, by: By, op: SetOp, values: &[&str]) -> Filter {
    Filter::Set { field, by, op, values: values.iter().map(|s| s.to_string()).collect() }
}

#[test]
fn a_blank_filter_is_no_filter() {
    assert_eq!(parse("").unwrap(), None);
    assert_eq!(parse("  \n ").unwrap(), None);
}

#[test]
fn set_filters_on_tags_categories_and_tools() {
    assert_eq!(ok(r#"tags.id IN ["da133078-247a-42db-a599-57fe46062075"]"#), set(SetField::Tags, By::Id, SetOp::In, &["da133078-247a-42db-a599-57fe46062075"]));
    assert_eq!(ok(r#"recipe_category.id CONTAINS ALL ["a", "b"]"#), set(SetField::Categories, By::Id, SetOp::ContainsAll, &["a", "b"]));
    assert_eq!(ok(r#"tools.name NOT IN ['wok']"#), set(SetField::Tools, By::Name, SetOp::NotIn, &["wok"]));
    // the other spellings Mealie's UI and API use
    assert_eq!(ok(r#"recipeCategory.id in ["1"]"#), set(SetField::Categories, By::Id, SetOp::In, &["1"]));
    assert_eq!(ok(r#"categories.name contains all ["x"]"#), set(SetField::Categories, By::Name, SetOp::ContainsAll, &["x"]));
    assert_eq!(ok(r#"tags.id IN [12, 13]"#), set(SetField::Tags, By::Id, SetOp::In, &["12", "13"]));
    assert_eq!(ok(r#"tags.id IN []"#), set(SetField::Tags, By::Id, SetOp::In, &[]));
}

#[test]
fn text_filters() {
    assert_eq!(ok(r#"name LIKE "%soup%""#), Filter::Text { field: TextField::Name, op: TextOp::Like, value: "%soup%".into() });
    assert_eq!(ok(r#"description NOT LIKE 'x'"#), Filter::Text { field: TextField::Description, op: TextOp::NotLike, value: "x".into() });
    assert_eq!(ok(r#"name = "Pancakes""#), Filter::Text { field: TextField::Name, op: TextOp::Eq, value: "Pancakes".into() });
    assert_eq!(ok(r#"name <> "Pancakes""#), Filter::Text { field: TextField::Name, op: TextOp::Ne, value: "Pancakes".into() });
    assert_eq!(ok(r#"name != "Pancakes""#), Filter::Text { field: TextField::Name, op: TextOp::Ne, value: "Pancakes".into() });
}

#[test]
fn string_escapes_keep_the_quote_inside_the_value() {
    assert_eq!(ok(r#"name LIKE "x\" OR 1=1 --""#), Filter::Text { field: TextField::Name, op: TextOp::Like, value: "x\" OR 1=1 --".into() });
    assert_eq!(ok(r#"name = 'it\'s \\ ok'"#), Filter::Text { field: TextField::Name, op: TextOp::Eq, value: "it's \\ ok".into() });
}

#[test]
fn null_date_and_rating_filters() {
    assert_eq!(ok("lastMade IS NULL"), Filter::Null { field: Field::LastMade, negated: false });
    assert_eq!(ok("last_made is not null"), Filter::Null { field: Field::LastMade, negated: true });
    assert_eq!(ok("rating IS NULL"), Filter::Null { field: Field::Rating, negated: false });
    assert_eq!(ok(r#"dateAdded >= "2024-01-31""#), Filter::Cmp { field: Field::DateAdded, op: CmpOp::Ge, value: Lit::Date("2024-01-31".into()) });
    assert_eq!(ok(r#"created_at < "2024-02-01""#), Filter::Cmp { field: Field::DateAdded, op: CmpOp::Lt, value: Lit::Date("2024-02-01".into()) });
    assert_eq!(ok(r#"updatedAt <= "2024-02-01T10:00:00""#), Filter::Cmp { field: Field::DateUpdated, op: CmpOp::Le, value: Lit::Date("2024-02-01".into()) });
    assert_eq!(ok(r#"lastMade > "2025-05-05""#), Filter::Cmp { field: Field::LastMade, op: CmpOp::Gt, value: Lit::Date("2025-05-05".into()) });
    assert_eq!(ok("rating > 3.5"), Filter::Cmp { field: Field::Rating, op: CmpOp::Gt, value: Lit::Num(3.5) });
    assert_eq!(ok("rating = 4"), Filter::Cmp { field: Field::Rating, op: CmpOp::Eq, value: Lit::Num(4.0) });
    assert_eq!(ok("rating <> 4"), Filter::Cmp { field: Field::Rating, op: CmpOp::Ne, value: Lit::Num(4.0) });
    assert_eq!(ok("rating <= 2"), Filter::Cmp { field: Field::Rating, op: CmpOp::Le, value: Lit::Num(2.0) });
}

#[test]
fn and_binds_tighter_than_or_and_parentheses_group() {
    let a = || set(SetField::Tags, By::Name, SetOp::In, &["a"]);
    let b = || set(SetField::Tags, By::Name, SetOp::In, &["b"]);
    let c = || set(SetField::Tools, By::Name, SetOp::In, &["c"]);
    assert_eq!(ok(r#"tags.name IN ["a"] OR tags.name IN ["b"] AND tools.name IN ["c"]"#), Filter::Or(vec![a(), Filter::And(vec![b(), c()])]));
    assert_eq!(ok(r#"(tags.name IN ["a"] OR tags.name IN ["b"]) AND tools.name IN ["c"]"#), Filter::And(vec![Filter::Or(vec![a(), b()]), c()]));
    assert_eq!(ok(r#"tags.name IN ["a"] AND tags.name IN ["b"] AND tools.name IN ["c"]"#), Filter::And(vec![a(), b(), c()]));
    assert_eq!(ok(r#"((tags.name IN ["a"]))"#), a());
}

#[test]
fn what_mealies_cookbook_builder_writes_parses() {
    let f = ok(r#"tags.id IN ["da133078-247a-42db-a599-57fe46062075"] AND recipe_category.id CONTAINS ALL ["f5b74eda-ad4b-4337-a6b5-309f039c5a67"] AND lastMade IS NULL"#);
    let Filter::And(parts) = f else { panic!("not an AND") };
    assert_eq!(parts.len(), 3);
}

#[test]
fn unsupported_input_is_an_error_naming_the_token() {
    for (src, token) in [
        (r#"household_id = "x""#, "household_id"),
        (r#"recipe_ingredient.food.id IN ["x"]"#, "recipe_ingredient.food.id"),
        (r#"name IN ["a"]"#, "IN"),
        (r#"rating LIKE "3""#, "LIKE"),
        (r#"tags.id LIKE "3""#, "LIKE"),
        (r#"tags IN ["a"]"#, "tags"),
        (r#"tags.slug IN ["a"]"#, "slug"),
        (r#"dateAdded > "yesterday""#, "yesterday"),
        (r#"lastMade > "2024-13""#, "2024-13"),
        (r#"rating > "high""#, "high"),
        (r#"name LIKE 5"#, "5"),
        (r#"tags.id IN "x""#, "\"x\""),
        (r#"tags.id IN ["a""#, "end of the filter"),
        (r#"name LIKE "abc"#, "unterminated"),
        (r#"name LIKE "a" foo"#, "foo"),
        (r#"name LIKE "a" AND"#, "end of the filter"),
        (r#"(name LIKE "a""#, "end of the filter"),
        (r#"name LIKE "a")"#, ")"),
        (r#"name LIKE "a"; DROP TABLE recipes"#, ";"),
        (r#"lastMade IS 5"#, "5"),
        (r#"name IS NULL"#, "IS"),
        (r#"rating >"#, "end of the filter"),
        (r#"AND name = "a""#, "AND"),
        (r#"tags.id CONTAINS ["a"]"#, "["),
    ] {
        let e = err(src);
        assert!(e.contains(token), "{src}: the error should name {token:?}: {e}");
        assert!(e.starts_with("filter not supported:"), "{e}");
    }
}

#[test]
fn absurd_nesting_and_length_are_refused_not_crashed_on() {
    let deep = format!("{}name = \"a\"{}", "(".repeat(500), ")".repeat(500));
    assert!(err(&deep).contains("nested"));
    let long = format!("name = \"{}\"", "a".repeat(9000));
    assert!(err(&long).contains("too long"));
}

// ---------- the evaluator ----------

use mealier::{Db, RecipeInput, Scope};

struct Set {
    db: Db,
    a: Scope,
    /// Another household of the same group.
    b: Scope,
    /// Another group.
    c: Scope,
    tag: i64,
}

async fn recipe(db: &Db, sc: &Scope, name: &str, description: &str, tags: &[&str], cats: &[&str], tools: &[&str]) -> i64 {
    let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    db.create_recipe(sc, RecipeInput { name: name.into(), description: description.into(), tags: v(tags), categories: v(cats), tools: v(tools), ..Default::default() }).await.unwrap().id
}

async fn made(db: &Db, sc: &Scope, recipe: i64, on: &str, rating: Option<i64>) {
    sqlx::query("INSERT INTO batches (household_id, status, recipe_id, source, made_on, rating) VALUES (?, 'done', ?, 'imported', ?, ?)")
        .bind(sc.household_id)
        .bind(recipe)
        .bind(on)
        .bind(rating)
        .execute(db.pool())
        .await
        .unwrap();
}

async fn world() -> Set {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let a = &t.a;
    let pancakes = recipe(&db, a, "Pancakes", "Fluffy", &["breakfast", "quick"], &["Mains"], &["pan"]).await;
    let soup = recipe(&db, a, "Tomato Soup", "Warm", &["quick", "vegan"], &["Soups"], &["pot"]).await;
    let stew = recipe(&db, a, "Beef Stew", "Slow and hearty", &["slow"], &["Mains", "Soups"], &["pot", "oven"]).await;
    recipe(&db, a, "Salad", "Fresh, 100% raw", &["vegan"], &["Sides"], &[]).await;
    let old = recipe(&db, a, "Old Thing", "", &["quick"], &[], &[]).await;
    db.archive_recipe(a, old).await.unwrap();
    recipe(&db, &t.c, "Pancakes Elsewhere", "", &["quick"], &["Mains"], &["pan"]).await;
    made(&db, a, pancakes, "2026-01-10", Some(5)).await;
    made(&db, a, pancakes, "2026-02-10", Some(3)).await;
    made(&db, a, soup, "2026-03-01", None).await;
    // another household's history of the stew is not ours
    made(&db, &t.b, stew, "2026-04-01", Some(1)).await;
    let tag = sqlx::query_scalar("SELECT id FROM tags WHERE group_id = ? AND name = 'quick'").bind(a.group_id).fetch_one(db.pool()).await.unwrap();
    Set { db, a: t.a, b: t.b, c: t.c, tag }
}

async fn names(w: &Set, src: &str) -> Vec<String> {
    names_as(w, &w.a, src).await
}

async fn recipe_id(w: &Set, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM recipes WHERE group_id = ? AND name = ?").bind(w.a.group_id).bind(name).fetch_one(w.db.pool()).await.unwrap()
}

async fn user_in(w: &Set, sc: &Scope, name: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO users (username, email, full_name, role, household_id) VALUES (?, ?, ?, 'user', ?) RETURNING id")
        .bind(name)
        .bind(format!("{name}@example.invalid"))
        .bind(name)
        .bind(sc.household_id)
        .fetch_one(w.db.pool())
        .await
        .unwrap()
}

async fn names_as(w: &Set, sc: &Scope, src: &str) -> Vec<String> {
    let f = ok(src);
    w.db.filter_recipes(sc, &f).await.unwrap().into_iter().map(|r| r.name).collect()
}

#[tokio::test]
async fn tags_categories_and_tools_by_name() {
    let w = world().await;
    assert_eq!(names(&w, r#"tags.name IN ["quick"]"#).await, ["Pancakes", "Tomato Soup"]);
    assert_eq!(names(&w, r#"tags.name IN ["slow", "vegan"]"#).await, ["Beef Stew", "Salad", "Tomato Soup"]);
    assert_eq!(names(&w, r#"tags.name NOT IN ["quick"]"#).await, ["Beef Stew", "Salad"]);
    assert_eq!(names(&w, r#"recipe_category.name CONTAINS ALL ["Mains", "Soups"]"#).await, ["Beef Stew"]);
    assert_eq!(names(&w, r#"recipe_category.name IN ["Mains", "Soups"]"#).await, ["Beef Stew", "Pancakes", "Tomato Soup"]);
    assert_eq!(names(&w, r#"tools.name CONTAINS ALL ["pot"]"#).await, ["Beef Stew", "Tomato Soup"]);
    assert_eq!(names(&w, r#"tools.name NOT IN ["pot", "pan"]"#).await, ["Salad"]);
    assert_eq!(names(&w, r#"tags.name IN []"#).await, Vec::<String>::new());
    assert_eq!(names(&w, r#"tags.name CONTAINS ALL []"#).await, ["Beef Stew", "Pancakes", "Salad", "Tomato Soup"]);
}

#[tokio::test]
async fn tags_by_id_resolve_inside_the_callers_group_only() {
    let w = world().await;
    assert_eq!(names(&w, &format!(r#"tags.id IN ["{}"]"#, w.tag)).await, ["Pancakes", "Tomato Soup"]);
    assert_eq!(names(&w, &format!(r#"tags.id IN [{}]"#, w.tag)).await, ["Pancakes", "Tomato Soup"]);
    // group Two's own "quick" tag has a different id; group One's id means nothing to it
    assert_eq!(names_as(&w, &w.c, &format!(r#"tags.id IN ["{}"]"#, w.tag)).await, Vec::<String>::new());
    assert_eq!(names_as(&w, &w.c, r#"tags.name IN ["quick"]"#).await, ["Pancakes Elsewhere"]);
    // an id nothing has, and a Mealie uuid: nothing matches, and NOT IN excludes nothing
    assert_eq!(names(&w, r#"tags.id IN ["0"]"#).await, Vec::<String>::new());
    assert_eq!(names(&w, r#"tags.id IN ["da133078-247a-42db-a599-57fe46062075"]"#).await, Vec::<String>::new());
    assert_eq!(names(&w, r#"tags.id NOT IN ["0"]"#).await, ["Beef Stew", "Pancakes", "Salad", "Tomato Soup"]);
}

#[tokio::test]
async fn text_filters_match_name_and_description() {
    let w = world().await;
    assert_eq!(names(&w, r#"name LIKE "%soup%""#).await, ["Tomato Soup"]);
    assert_eq!(names(&w, r#"name LIKE "pancakes""#).await, ["Pancakes"]);
    assert_eq!(names(&w, r#"name NOT LIKE "%e%""#).await, ["Salad", "Tomato Soup"]);
    assert_eq!(names(&w, r#"name = "Salad""#).await, ["Salad"]);
    assert_eq!(names(&w, r#"name <> "Salad""#).await, ["Beef Stew", "Pancakes", "Tomato Soup"]);
    assert_eq!(names(&w, r#"description LIKE "%hearty%""#).await, ["Beef Stew"]);
    // `=` is exact: the % in the stored description is data, not a wildcard
    assert_eq!(names(&w, r#"description = "Fresh, 100% raw""#).await, ["Salad"]);
}

#[tokio::test]
async fn last_made_and_rating_read_the_callers_household() {
    let w = world().await;
    assert_eq!(names(&w, "lastMade IS NULL").await, ["Beef Stew", "Salad"]);
    assert_eq!(names(&w, "lastMade IS NOT NULL").await, ["Pancakes", "Tomato Soup"]);
    assert_eq!(names(&w, r#"lastMade > "2026-02-15""#).await, ["Tomato Soup"]);
    assert_eq!(names(&w, r#"lastMade <= "2026-02-10""#).await, ["Pancakes"]);
    assert_eq!(names(&w, r#"lastMade = "2026-03-01""#).await, ["Tomato Soup"]);
    // `rating` is the caller's own rating (Mealie semantics), not the batches' average:
    // household a's batches average 4 for the pancakes, but this user rates them 2.
    let pancakes = recipe_id(&w, "Pancakes").await;
    let salad = recipe_id(&w, "Salad").await;
    let ann = Scope { user_id: user_in(&w, &w.a, "ann").await, ..w.a };
    let bob = Scope { user_id: user_in(&w, &w.b, "bob").await, ..w.b };
    w.db.set_rating(&ann, pancakes, Some(2.0)).await.unwrap();
    w.db.set_rating(&bob, salad, Some(1.0)).await.unwrap();
    assert_eq!(names_as(&w, &ann, "rating >= 4").await, Vec::<String>::new());
    assert_eq!(names_as(&w, &ann, "rating < 4").await, ["Pancakes"]);
    assert_eq!(names_as(&w, &ann, "rating IS NULL").await, ["Beef Stew", "Salad", "Tomato Soup"]);
    // household b made the stew and rated that batch 1, but b's user rated only the salad
    assert_eq!(names_as(&w, &bob, "lastMade IS NOT NULL").await, ["Beef Stew"]);
    assert_eq!(names_as(&w, &bob, "rating <= 1").await, ["Salad"]);
}

#[tokio::test]
async fn date_added_and_updated_compare_by_day() {
    let w = world().await;
    let all = ["Beef Stew", "Pancakes", "Salad", "Tomato Soup"];
    assert_eq!(names(&w, r#"dateAdded > "2000-01-01""#).await, all);
    assert_eq!(names(&w, r#"dateAdded < "2000-01-01""#).await, Vec::<String>::new());
    assert_eq!(names(&w, r#"dateUpdated >= "2000-01-01" AND dateUpdated < "2999-01-01""#).await, all);
    assert_eq!(names(&w, "dateAdded IS NOT NULL").await, all);
}

#[tokio::test]
async fn and_or_and_parentheses_combine() {
    let w = world().await;
    assert_eq!(names(&w, r#"tags.name IN ["quick"] AND lastMade > "2026-02-15""#).await, ["Tomato Soup"]);
    assert_eq!(names(&w, r#"tags.name IN ["slow"] OR name LIKE "salad""#).await, ["Beef Stew", "Salad"]);
    assert_eq!(names(&w, r#"(tags.name IN ["slow"] OR tags.name IN ["vegan"]) AND tools.name IN ["pot"]"#).await, ["Beef Stew", "Tomato Soup"]);
    assert_eq!(names(&w, r#"tags.name IN ["slow"] OR tags.name IN ["vegan"] AND tools.name IN ["pot"]"#).await, ["Beef Stew", "Tomato Soup"]);
    assert_eq!(names(&w, r#"tags.name IN ["vegan"] AND tools.name NOT IN ["pot"] AND rating IS NULL"#).await, ["Salad"]);
}

#[tokio::test]
async fn archived_recipes_and_other_groups_never_match() {
    let w = world().await;
    assert!(!names(&w, "name LIKE \"%\"").await.contains(&"Old Thing".to_string()));
    assert!(!names(&w, "name LIKE \"%\"").await.contains(&"Pancakes Elsewhere".to_string()));
    assert_eq!(names_as(&w, &w.c, "name LIKE \"%\"").await, ["Pancakes Elsewhere"]);
}

#[tokio::test]
async fn values_that_look_like_sql_are_just_values() {
    let w = world().await;
    for src in [
        r#"name LIKE "x\" OR 1=1 --""#,
        r#"name = "x' OR '1'='1""#,
        r#"name = "'; DROP TABLE recipes; --""#,
        r#"tags.name IN ["quick') OR 1=1 --", "x"]"#,
        r#"tags.id IN ["0 OR 1=1"]"#,
        r#"tools.name CONTAINS ALL ["pot\") OR (1=1"]"#,
        r#"lastMade > "2026-01-01" AND name LIKE "\" OR \"1\"=\"1""#,
    ] {
        assert_eq!(names(&w, src).await, Vec::<String>::new(), "{src}");
    }
    // the hostile value reaches the database as a value only: nothing was dropped or changed
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM recipes").fetch_one(w.db.pool()).await.unwrap();
    assert_eq!(n, 6);
    assert_eq!(names(&w, r#"name LIKE "%""#).await, ["Beef Stew", "Pancakes", "Salad", "Tomato Soup"]);
}
