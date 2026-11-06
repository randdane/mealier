//! Meal-plan rules (step 13): the store, specificity, household limiting and the seeded suggestion.
mod common;
use std::collections::BTreeSet;

use mealier::{Db, Error, RecipeInput, Rng, RuleInput, Scope};

// 2026-11-02 is a Monday.
const MON: &str = "2026-11-02";
const TUE: &str = "2026-11-03";

fn rule(day: &str, slot: &str, filter: &str, households: &[i64]) -> RuleInput {
    RuleInput { day: day.into(), slot: slot.into(), query_filter: filter.into(), household_ids: households.into() }
}

async fn recipe(db: &Db, sc: &Scope, name: &str, tags: &[&str]) -> i64 {
    db.create_recipe(sc, RecipeInput { name: name.into(), tags: tags.iter().map(|t| t.to_string()).collect(), ..Default::default() }).await.unwrap().id
}

/// What 40 different seeds pick: every candidate should turn up, and nothing else.
async fn picks(db: &Db, sc: &Scope, date: &str, slot: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for seed in 0..40 {
        if let Some(r) = db.suggest(sc, date, slot, &mut Rng::seeded(seed)).await.unwrap() {
            out.insert(r.name);
        }
    }
    out
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_generator_is_deterministic_and_stays_in_range() {
    let draw = |seed| {
        let mut r = Rng::seeded(seed);
        (0..50).map(|_| r.below(7)).collect::<Vec<_>>()
    };
    assert_eq!(draw(1), draw(1));
    assert_ne!(draw(1), draw(2));
    assert!(draw(3).iter().all(|n| *n < 7));
    assert_eq!(draw(4).iter().copied().collect::<BTreeSet<_>>().len(), 7, "all of 0..7 turn up");
    assert_eq!(Rng::seeded(9).below(1), 0);
}

#[tokio::test]
async fn a_rule_round_trips() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let r = db.add_rule(&t.a, rule("monday", "dinner", r#"tags.name IN ["quick"]"#, &[t.a.household_id])).await.unwrap();
    assert_eq!((r.day.as_str(), r.slot.as_str(), r.query_filter.as_str(), r.filter_supported, r.household_ids.as_slice()), ("monday", "dinner", r#"tags.name IN ["quick"]"#, true, &[t.a.household_id][..]));
    let any = db.add_rule(&t.a, RuleInput::default()).await.unwrap();
    assert_eq!((any.day.as_str(), any.slot.as_str(), any.household_ids.len()), ("unset", "unset", 0));
    assert_eq!(db.rule(&t.b, r.id).await.unwrap(), r, "a household that is not in its list still reads it: rules are group-wide");
    assert_eq!(db.rules(&t.a).await.unwrap(), vec![r.clone(), any.clone()]);

    let r2 = db.update_rule(&t.b, r.id, rule("unset", "lunch", "", &[t.b.household_id, t.b.household_id, t.a.household_id])).await.unwrap();
    let mut want = vec![t.a.household_id, t.b.household_id];
    want.sort();
    assert_eq!((r2.day.as_str(), r2.slot.as_str(), r2.query_filter.as_str(), r2.household_ids), ("unset", "lunch", "", want));
    db.delete_rule(&t.a, r.id).await.unwrap();
    assert!(matches!(db.rule(&t.a, r.id).await, Err(Error::NotFound(_))));
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM meal_plan_rule_households").fetch_one(db.pool()).await.unwrap(), 0, "the links go with the rule");
}

#[tokio::test]
async fn bad_rules_are_refused() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    for (input, want) in [
        (rule("someday", "", "", &[]), "day"),
        (rule("", "brunch", "", &[]), "slot"),
        (rule("", "", "name IN [", &[]), "filter not supported"),
        (rule("", "", r#"tags.id IN ["abc"]"#, &[]), "not a mealier id"),
    ] {
        match db.add_rule(&t.a, input).await {
            Err(Error::Validation(m)) => assert!(m.contains(want), "{want}: {m}"),
            other => panic!("{want}: {other:?}"),
        }
    }
    // a household of another group is not found, and nothing is written
    assert!(matches!(db.add_rule(&t.a, rule("", "", "", &[t.c.household_id])).await, Err(Error::NotFound(_))));
    assert!(matches!(db.add_rule(&t.a, rule("", "", "", &[9999])).await, Err(Error::NotFound(_))));
    let ok = db.add_rule(&t.a, rule("", "", "", &[])).await.unwrap();
    assert!(matches!(db.update_rule(&t.a, ok.id, rule("", "", "", &[t.c.household_id])).await, Err(Error::NotFound(_))));
    assert_eq!(db.rule(&t.a, ok.id).await.unwrap(), ok);
    assert_eq!(db.rules(&t.a).await.unwrap().len(), 1);
}

#[tokio::test]
async fn rules_belong_to_the_group() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let r = db.add_rule(&t.a, RuleInput::default()).await.unwrap();
    assert!(db.rules(&t.c).await.unwrap().is_empty());
    assert!(matches!(db.rule(&t.c, r.id).await, Err(Error::NotFound(_))));
    assert!(matches!(db.update_rule(&t.c, r.id, RuleInput::default()).await, Err(Error::NotFound(_))));
    assert!(matches!(db.delete_rule(&t.c, r.id).await, Err(Error::NotFound(_))));
    assert_eq!(db.rules(&t.b).await.unwrap(), vec![r]);
}

#[tokio::test]
async fn the_most_specific_applicable_rule_wins() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    for (name, tag) in [("Any Dish", "a"), ("Monday Dish", "d"), ("Dinner Dish", "s"), ("Monday Dinner Dish", "b"), ("Unlisted Dish", "u")] {
        recipe(&db, &sc, name, &[tag]).await;
    }
    for (day, slot, tag) in [("unset", "unset", "a"), ("monday", "unset", "d"), ("unset", "dinner", "s"), ("monday", "dinner", "b")] {
        db.add_rule(&sc, rule(day, slot, &format!(r#"tags.name IN ["{tag}"]"#), &[])).await.unwrap();
    }
    assert_eq!(picks(&db, &sc, MON, "dinner").await, set(&["Monday Dinner Dish"]), "day and slot beat day and slot alone");
    assert_eq!(picks(&db, &sc, MON, "lunch").await, set(&["Monday Dish"]), "day beats slot and any");
    assert_eq!(picks(&db, &sc, TUE, "dinner").await, set(&["Dinner Dish"]), "slot beats any");
    assert_eq!(picks(&db, &sc, TUE, "lunch").await, set(&["Any Dish"]));
}

#[tokio::test]
async fn rules_of_one_specificity_are_pooled_and_the_pick_is_seeded() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    for (name, tag) in [("One", "x"), ("Two", "x"), ("Three", "y"), ("Four", "z")] {
        recipe(&db, &sc, name, &[tag]).await;
    }
    db.add_rule(&sc, rule("", "", r#"tags.name IN ["x"]"#, &[])).await.unwrap();
    db.add_rule(&sc, rule("", "", r#"tags.name IN ["y"]"#, &[])).await.unwrap();
    assert_eq!(picks(&db, &sc, MON, "dinner").await, set(&["One", "Two", "Three"]));
    let first = db.suggest(&sc, MON, "dinner", &mut Rng::seeded(7)).await.unwrap();
    let again = db.suggest(&sc, MON, "dinner", &mut Rng::seeded(7)).await.unwrap();
    assert_eq!(first, again, "the same seed gives the same recipe");
    assert!(first.is_some());
}

#[tokio::test]
async fn nothing_applies_nothing_matches_or_nothing_readable_is_none() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    recipe(&db, &sc, "Dish", &["x"]).await;
    assert_eq!(db.suggest(&sc, MON, "dinner", &mut Rng::seeded(1)).await.unwrap(), None, "no rules at all");
    // a rule for another day does not apply
    db.add_rule(&sc, rule("tuesday", "", "", &[])).await.unwrap();
    assert_eq!(db.suggest(&sc, MON, "dinner", &mut Rng::seeded(1)).await.unwrap(), None);
    assert_eq!(picks(&db, &sc, TUE, "dinner").await, set(&["Dish"]), "an empty filter selects every recipe");
    // the most specific rule selects nothing: no fall back to a looser one
    db.add_rule(&sc, rule("", "", "", &[])).await.unwrap();
    db.add_rule(&sc, rule("monday", "", r#"tags.name IN ["nope"]"#, &[])).await.unwrap();
    assert_eq!(db.suggest(&sc, MON, "dinner", &mut Rng::seeded(1)).await.unwrap(), None);
    // a stored filter mealier cannot read selects nothing
    sqlx::query("INSERT INTO meal_plan_rules (group_id, day, query_filter, filter_json) VALUES (?, 'wednesday', 'household_id = \"x\"', NULL)").bind(sc.group_id).execute(db.pool()).await.unwrap();
    assert_eq!(db.suggest(&sc, "2026-11-04", "dinner", &mut Rng::seeded(1)).await.unwrap(), None);
    // archived recipes are never suggested
    let only = recipe(&db, &sc, "Gone", &["gone"]).await;
    db.add_rule(&sc, rule("friday", "", r#"tags.name IN ["gone"]"#, &[])).await.unwrap();
    db.archive_recipe(&sc, only).await.unwrap();
    assert_eq!(db.suggest(&sc, "2026-11-06", "dinner", &mut Rng::seeded(1)).await.unwrap(), None);
}

#[tokio::test]
async fn a_rule_limited_to_households_applies_only_to_them() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    recipe(&db, &t.a, "For Alpha", &["alpha"]).await;
    recipe(&db, &t.a, "For All", &["all"]).await;
    db.add_rule(&t.a, rule("monday", "", r#"tags.name IN ["alpha"]"#, &[t.a.household_id])).await.unwrap();
    db.add_rule(&t.a, rule("", "", r#"tags.name IN ["all"]"#, &[])).await.unwrap();
    assert_eq!(picks(&db, &t.a, MON, "dinner").await, set(&["For Alpha"]), "alpha's own, more specific rule");
    assert_eq!(picks(&db, &t.b, MON, "dinner").await, set(&["For All"]), "beta falls to the rule for everyone");
    assert_eq!(picks(&db, &t.b, TUE, "dinner").await, set(&["For All"]));
    // a rule limited to a household that is not the caller's is not theirs
    let only_alpha = db.add_rule(&t.a, rule("tuesday", "", "", &[t.a.household_id])).await.unwrap();
    assert_eq!(picks(&db, &t.b, TUE, "dinner").await, set(&["For All"]));
    db.update_rule(&t.a, only_alpha.id, rule("tuesday", "", "", &[t.a.household_id, t.b.household_id])).await.unwrap();
    assert_eq!(picks(&db, &t.b, TUE, "dinner").await, set(&["For Alpha", "For All"]));
}

#[tokio::test]
async fn another_groups_rules_and_recipes_never_leak_into_a_suggestion() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    recipe(&db, &t.a, "Ours", &["x"]).await;
    recipe(&db, &t.c, "Theirs", &["x"]).await;
    db.add_rule(&t.c, rule("", "", "", &[])).await.unwrap();
    assert_eq!(picks(&db, &t.a, MON, "dinner").await, set(&[]), "group Two's rule says nothing about group One");
    db.add_rule(&t.a, rule("", "", r#"tags.name IN ["x"]"#, &[])).await.unwrap();
    assert_eq!(picks(&db, &t.a, MON, "dinner").await, set(&["Ours"]));
    assert_eq!(picks(&db, &t.c, MON, "dinner").await, set(&["Theirs"]));
}

#[tokio::test]
async fn a_bad_date_or_slot_is_a_validation_error() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    for (date, slot) in [("2026-13-01", "dinner"), ("tomorrow", "dinner"), (MON, "brunch"), (MON, "unset")] {
        assert!(matches!(db.suggest(&sc, date, slot, &mut Rng::seeded(1)).await, Err(Error::Validation(_))), "{date} {slot}");
    }
}
