//! Step 14, part B: favourites and ratings. Per user and recipe; the rating is a float in half steps.
mod common;
use mealier::{Db, Error, RecipeInput, Scope};

/// A real user in the default household (the store's foreign key wants one).
async fn user(db: &Db, name: &str, household_id: i64, group_id: i64) -> Scope {
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES (?, ?, 'user', ?) RETURNING id")
        .bind(name)
        .bind(format!("{name}@x"))
        .bind(household_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    Scope { user_id: id, group_id, household_id, admin: false }
}

async fn recipe(db: &Db, sc: &Scope, name: &str) -> i64 {
    db.create_recipe(sc, RecipeInput { name: name.into(), ..Default::default() }).await.unwrap().id
}

#[tokio::test]
async fn a_new_user_has_no_opinion_and_favourites_and_ratings_are_independent() {
    let db = Db::open_memory().await.unwrap();
    let base = common::scope(&db).await;
    let ann = user(&db, "ann", base.household_id, base.group_id).await;
    let r = recipe(&db, &ann, "Soup").await;
    assert_eq!(db.opinion(&ann, r).await.unwrap().rating, None);
    assert!(!db.opinion(&ann, r).await.unwrap().favourite);

    db.set_favourite(&ann, r, true).await.unwrap();
    let o = db.opinion(&ann, r).await.unwrap();
    assert!(o.favourite && o.rating.is_none(), "{o:?}");
    db.set_rating(&ann, r, Some(3.5)).await.unwrap();
    let o = db.opinion(&ann, r).await.unwrap();
    assert!(o.favourite && o.rating == Some(3.5), "rating leaves the favourite alone: {o:?}");
    db.set_favourite(&ann, r, false).await.unwrap();
    assert_eq!(db.opinion(&ann, r).await.unwrap().rating, Some(3.5), "unfavouriting keeps the rating");
    db.set_rating(&ann, r, None).await.unwrap();
    assert_eq!(db.opinion(&ann, r).await.unwrap().rating, None);
}

#[tokio::test]
async fn ratings_are_between_half_a_star_and_five_in_half_steps() {
    let db = Db::open_memory().await.unwrap();
    let base = common::scope(&db).await;
    let ann = user(&db, "ann", base.household_id, base.group_id).await;
    let r = recipe(&db, &ann, "Soup").await;
    for ok in [0.5, 1.0, 4.5, 5.0] {
        db.set_rating(&ann, r, Some(ok)).await.unwrap();
    }
    for bad in [0.0, 0.4, 5.5, 3.7, -1.0, f64::NAN, f64::INFINITY] {
        match db.set_rating(&ann, r, Some(bad)).await {
            Err(Error::Validation(_)) => {}
            other => panic!("{bad}: {other:?}"),
        }
    }
    assert_eq!(db.opinion(&ann, r).await.unwrap().rating, Some(5.0), "refusals changed nothing");
}

#[tokio::test]
async fn two_users_of_one_household_keep_their_own_and_favourites_list_only_mine() {
    let db = Db::open_memory().await.unwrap();
    let base = common::scope(&db).await;
    let (ann, bo) = (user(&db, "ann", base.household_id, base.group_id).await, user(&db, "bo", base.household_id, base.group_id).await);
    let (soup, stew) = (recipe(&db, &ann, "Soup").await, recipe(&db, &ann, "Stew").await);
    db.set_favourite(&ann, soup, true).await.unwrap();
    db.set_rating(&ann, soup, Some(5.0)).await.unwrap();
    db.set_favourite(&bo, stew, true).await.unwrap();
    assert_eq!(db.opinion(&bo, soup).await.unwrap().rating, None);
    assert_eq!(db.favourite_recipes(&ann).await.unwrap().iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Soup"]);
    assert_eq!(db.favourite_recipes(&bo).await.unwrap().iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Stew"]);
}

#[tokio::test]
async fn a_recipe_of_another_group_is_not_found_and_nothing_is_written() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let ann = user(&db, "ann", t.a.household_id, t.a.group_id).await;
    let cy = user(&db, "cy", t.c.household_id, t.c.group_id).await;
    let r = recipe(&db, &ann, "Soup").await;
    assert!(matches!(db.set_favourite(&cy, r, true).await, Err(Error::NotFound(_))));
    assert!(matches!(db.set_rating(&cy, r, Some(3.0)).await, Err(Error::NotFound(_))));
    assert!(matches!(db.opinion(&cy, r).await, Err(Error::NotFound(_))));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM user_recipes").fetch_one(db.pool()).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn favourites_leave_out_archived_recipes() {
    let db = Db::open_memory().await.unwrap();
    let base = common::scope(&db).await;
    let ann = user(&db, "ann", base.household_id, base.group_id).await;
    let r = recipe(&db, &ann, "Soup").await;
    db.set_favourite(&ann, r, true).await.unwrap();
    db.archive_recipe(&ann, r).await.unwrap();
    assert!(db.favourite_recipes(&ann).await.unwrap().is_empty());
}
