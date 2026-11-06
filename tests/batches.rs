mod common;
use mealier::{BatchInput, BatchLineInput, Db, Dimension, Error, IngredientInput, RecipeInput};

/// Pancakes: 250 g flour, 1-2 eggs (count), salt to taste (no amount).
async fn pancakes(db: &Db) -> (mealier::Recipe, i64, i64, i64) {
    let g = db.add_unit(&common::scope(db).await, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    let flour = db.add_food(&common::scope(db).await, "flour").await.unwrap();
    let egg = db.add_food(&common::scope(db).await, "egg").await.unwrap();
    let salt = db.add_food(&common::scope(db).await, "salt").await.unwrap();
    let ing = |qty: Option<(f64, f64)>, unit, food| IngredientInput {
        qty_min: qty.map(|q| q.0),
        qty_max: qty.map(|q| q.1),
        unit_id: unit,
        food_id: Some(food),
        ..Default::default()
    };
    let recipe = db
        .create_recipe(&common::scope(db).await, RecipeInput {
            name: "Pancakes".into(),
            ingredients: vec![ing(Some((250.0, 250.0)), Some(g), flour), ing(Some((1.0, 2.0)), None, egg), ing(None, None, salt)],
            ..Default::default()
        })
        .await
        .unwrap();
    (recipe, g, flour, egg)
}

#[tokio::test]
async fn start_batch_is_planned_on_the_current_snapshot_with_scaled_actuals() {
    let db = Db::open_memory().await.unwrap();
    let (r, g, _, _) = pancakes(&db).await;

    let b = db.start_batch(&common::scope(&db).await, r.id, 2.0).await.unwrap();

    assert_eq!(b.batch.status, "planned");
    assert_eq!(b.batch.source, "native");
    assert_eq!(b.batch.snapshot_id, Some(r.current_snapshot_id));
    assert_eq!(b.batch.scale, 2.0);
    assert_eq!(b.batch.made_on, None);
    assert_eq!(b.lines.len(), 3);
    let l: Vec<_> = b.lines.iter().map(|l| (l.qty_min, l.qty_max, l.unit_id, l.food_name.as_deref(), l.added)).collect();
    assert_eq!(
        l,
        vec![
            (Some(500.0), Some(500.0), Some(g), Some("flour"), false),
            (Some(2.0), Some(4.0), None, Some("egg"), false),
            (None, None, None, Some("salt"), false),
        ]
    );
    assert_eq!(b.lines[0].unit_name.as_deref(), Some("g"));
}

#[tokio::test]
async fn start_batch_for_a_missing_recipe_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    assert!(matches!(db.start_batch(&common::scope(&db).await, 99, 1.0).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn start_batch_with_scale_zero_or_less_is_a_validation_error() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    assert!(matches!(db.start_batch(&common::scope(&db).await, r.id, 0.0).await, Err(Error::Validation(_))));
    assert!(matches!(db.start_batch(&common::scope(&db).await, r.id, -1.0).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn get_batch_shows_scaled_recipe_amounts_from_its_snapshot_even_after_the_recipe_changes() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 2.0).await.unwrap();
    db.update_recipe(&common::scope(&db).await, r.id, RecipeInput { name: "Other".into(), ..Default::default() }).await.unwrap();

    let got = db.get_batch(&common::scope(&db).await, b.batch.id).await.unwrap();

    let l: Vec<_> = got.lines.iter().map(|l| (l.ingredient.as_str(), l.recipe_qty_min, l.recipe_qty_max)).collect();
    assert_eq!(l, vec![("flour", Some(500.0), Some(500.0)), ("egg", Some(2.0), Some(4.0)), ("salt", None, None)]);
    assert_eq!(got.lines[0].recipe_unit_name.as_deref(), Some("g"));
    assert!(matches!(db.get_batch(&common::scope(&db).await, 999).await, Err(Error::NotFound(_))));
}

/// The input that changes nothing: current values of every line.
fn same(b: &mealier::BatchDetail) -> BatchInput {
    BatchInput {
        made_on: b.batch.made_on.clone(),
        scale: b.batch.scale,
        rating: b.batch.rating,
        notes: b.batch.notes.clone(),
        lines: b
            .lines
            .iter()
            .map(|l| BatchLineInput {
                id: Some(l.id),
                qty_min: l.qty_min,
                qty_max: l.qty_max,
                unit_id: l.unit_id,
                food_id: l.food_id,
                substitution: l.substitution.clone(),
                substitute_food_id: l.substitute_food_id,
            })
            .collect(),
    }
}

#[tokio::test]
async fn update_batch_saves_header_fields_and_per_line_actuals() {
    let db = Db::open_memory().await.unwrap();
    let (r, g, _, _) = pancakes(&db).await;
    let ghee = db.add_food(&common::scope(&db).await, "ghee").await.unwrap();
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();

    let mut input = same(&b);
    input.made_on = Some("2026-11-01".into());
    input.scale = 1.5;
    input.rating = Some(4);
    input.notes = "a bit dry".into();
    input.lines[0].qty_min = Some(240.0);
    input.lines[0].qty_max = Some(240.0);
    input.lines[0].unit_id = Some(g);
    input.lines[1].qty_min = Some(3.0);
    input.lines[1].qty_max = None;
    input.lines[1].substitution = "duck eggs".into();
    input.lines[1].substitute_food_id = Some(ghee);
    let got = db.update_batch(&common::scope(&db).await, b.batch.id, input).await.unwrap();

    assert_eq!(got.batch.made_on.as_deref(), Some("2026-11-01"));
    assert_eq!(got.batch.scale, 1.5);
    assert_eq!(got.batch.rating, Some(4));
    assert_eq!(got.batch.notes, "a bit dry");
    assert_eq!((got.lines[0].qty_min, got.lines[0].qty_max), (Some(240.0), Some(240.0)));
    assert_eq!((got.lines[1].qty_min, got.lines[1].qty_max), (Some(3.0), Some(3.0)), "a lone qty_min is not a range");
    assert_eq!(got.lines[1].substitution, "duck eggs");
    assert_eq!(got.lines[1].substitute_food_name.as_deref(), Some("ghee"));
    assert_eq!(got.lines[2].qty_min, None, "unknown stays unknown");
    // recipe amounts follow the new scale
    assert_eq!(got.lines[0].recipe_qty_min, Some(375.0));
    // and it persisted
    assert_eq!(db.get_batch(&common::scope(&db).await, b.batch.id).await.unwrap(), got);
}

#[tokio::test]
async fn update_batch_adds_lines_flagged_added_and_may_remove_them_again() {
    let db = Db::open_memory().await.unwrap();
    let (r, g, _, _) = pancakes(&db).await;
    let milk = db.add_food(&common::scope(&db).await, "milk").await.unwrap();
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();

    let mut input = same(&b);
    input.lines.push(BatchLineInput { qty_min: Some(100.0), unit_id: Some(g), food_id: Some(milk), ..Default::default() });
    let got = db.update_batch(&common::scope(&db).await, b.batch.id, input).await.unwrap();
    assert_eq!(got.lines.len(), 4);
    let extra = got.lines.last().unwrap();
    assert!(extra.added);
    assert_eq!((extra.qty_min, extra.qty_max, extra.food_name.as_deref()), (Some(100.0), Some(100.0), Some("milk")));
    assert_eq!((extra.recipe_qty_min, extra.recipe_unit_name.clone()), (None, None));
    assert!(got.lines[..3].iter().all(|l| !l.added));
    assert_eq!(db.snapshots(&common::scope(&db).await, r.id).await.unwrap().len(), 1, "no new snapshot");

    let mut input = same(&got);
    input.lines.pop();
    let got = db.update_batch(&common::scope(&db).await, b.batch.id, input).await.unwrap();
    assert_eq!(got.lines.len(), 3);
}

#[tokio::test]
async fn update_batch_cannot_remove_a_snapshot_line() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    let mut input = same(&b);
    input.lines.remove(0);
    assert!(matches!(db.update_batch(&common::scope(&db).await, b.batch.id, input).await, Err(Error::Validation(_))));
    assert_eq!(db.get_batch(&common::scope(&db).await, b.batch.id).await.unwrap().lines.len(), 3);
}

#[tokio::test]
async fn update_batch_rejects_invalid_values_and_changes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    type Edit = Box<dyn Fn(&mut BatchInput)>;
    let bad: Vec<(&str, Edit)> = vec![
        ("rating 0", Box::new(|i| i.rating = Some(0))),
        ("rating 6", Box::new(|i| i.rating = Some(6))),
        ("scale 0", Box::new(|i| i.scale = 0.0)),
        ("scale negative", Box::new(|i| i.scale = -2.0)),
        ("negative qty", Box::new(|i| i.lines[0].qty_min = Some(-1.0))),
        ("reversed range", Box::new(|i| (i.lines[0].qty_min, i.lines[0].qty_max) = (Some(5.0), Some(2.0)))),
        ("max without min", Box::new(|i| (i.lines[2].qty_min, i.lines[2].qty_max) = (None, Some(2.0)))),
        ("bad date", Box::new(|i| i.made_on = Some("yesterday".into()))),
    ];
    for (what, edit) in bad {
        let mut input = same(&b);
        input.notes = "changed".into();
        edit(&mut input);
        let err = db.update_batch(&common::scope(&db).await, b.batch.id, input).await.unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{what}: {err:?}");
    }
    assert_eq!(db.get_batch(&common::scope(&db).await, b.batch.id).await.unwrap(), b);
}

#[tokio::test]
async fn update_batch_unknown_batch_unit_food_or_line_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    assert!(matches!(db.update_batch(&common::scope(&db).await, 999, same(&b)).await, Err(Error::NotFound(_))));
    let mut i = same(&b);
    i.lines[0].unit_id = Some(999);
    assert!(matches!(db.update_batch(&common::scope(&db).await, b.batch.id, i).await, Err(Error::NotFound("unit"))));
    let mut i = same(&b);
    i.lines[0].substitute_food_id = Some(999);
    assert!(matches!(db.update_batch(&common::scope(&db).await, b.batch.id, i).await, Err(Error::NotFound("food"))));
    let mut i = same(&b);
    i.lines.push(BatchLineInput { id: Some(9999), ..Default::default() });
    assert!(matches!(db.update_batch(&common::scope(&db).await, b.batch.id, i).await, Err(Error::NotFound("batch line"))));
}

#[tokio::test]
async fn add_batch_line_appends_an_added_line() {
    let db = Db::open_memory().await.unwrap();
    let (r, g, _, _) = pancakes(&db).await;
    let milk = db.add_food(&common::scope(&db).await, "milk").await.unwrap();
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();

    let got = db
        .add_batch_line(&common::scope(&db).await, b.batch.id, BatchLineInput { qty_min: Some(50.0), unit_id: Some(g), food_id: Some(milk), ..Default::default() })
        .await
        .unwrap();

    assert_eq!(got.lines.len(), 4);
    let l = &got.lines[3];
    assert!(l.added);
    assert_eq!((l.qty_min, l.qty_max, l.unit_id, l.food_name.as_deref()), (Some(50.0), Some(50.0), Some(g), Some("milk")));
    assert!(matches!(db.add_batch_line(&common::scope(&db).await, 999, BatchLineInput::default()).await, Err(Error::NotFound("batch"))));
    let bad = BatchLineInput { qty_min: Some(-1.0), ..Default::default() };
    assert!(matches!(db.add_batch_line(&common::scope(&db).await, b.batch.id, bad).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn finish_batch_marks_it_done_and_defaults_made_on_to_today() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();

    let done = db.finish_batch(&common::scope(&db).await, b.batch.id).await.unwrap();

    assert_eq!(done.batch.status, "done");
    let today: String = sqlx::query_scalar("SELECT date('now')").fetch_one(db.pool()).await.unwrap();
    assert_eq!(done.batch.made_on, Some(today));
    assert!(matches!(db.finish_batch(&common::scope(&db).await, 999).await, Err(Error::NotFound("batch"))));
}

#[tokio::test]
async fn finish_batch_keeps_a_made_on_that_was_set() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    let mut input = same(&b);
    input.made_on = Some("2026-10-31".into());
    db.update_batch(&common::scope(&db).await, b.batch.id, input).await.unwrap();

    let done = db.finish_batch(&common::scope(&db).await, b.batch.id).await.unwrap();

    assert_eq!(done.batch.made_on.as_deref(), Some("2026-10-31"));
}

#[tokio::test]
async fn finishing_twice_is_a_validation_error() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    db.finish_batch(&common::scope(&db).await, b.batch.id).await.unwrap();
    assert!(matches!(db.finish_batch(&common::scope(&db).await, b.batch.id).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn a_done_batch_can_still_be_edited() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    let done = db.finish_batch(&common::scope(&db).await, b.batch.id).await.unwrap();
    let mut input = same(&done);
    input.notes = "typo fixed".into();
    let got = db.update_batch(&common::scope(&db).await, b.batch.id, input).await.unwrap();
    assert_eq!((got.batch.status.as_str(), got.batch.notes.as_str()), ("done", "typo fixed"));
}

#[tokio::test]
async fn list_batches_is_newest_first() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let old = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    let mut i = same(&old);
    i.made_on = Some("2026-01-01".into());
    db.update_batch(&common::scope(&db).await, old.batch.id, i).await.unwrap();
    let new = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    let mut i = same(&new);
    i.made_on = Some("2026-06-01".into());
    db.update_batch(&common::scope(&db).await, new.batch.id, i).await.unwrap();

    let ids: Vec<i64> = db.list_batches(&common::scope(&db).await, r.id).await.unwrap().iter().map(|b| b.id).collect();

    assert_eq!(ids, vec![new.batch.id, old.batch.id]);
}

#[tokio::test]
async fn list_batches_puts_undated_planned_batches_first() {
    let db = Db::open_memory().await.unwrap();
    let (r, ..) = pancakes(&db).await;
    let dated = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    let mut i = same(&dated);
    i.made_on = Some("2026-06-01".into());
    db.update_batch(&common::scope(&db).await, dated.batch.id, i).await.unwrap();
    let planned = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();

    let ids: Vec<i64> = db.list_batches(&common::scope(&db).await, r.id).await.unwrap().iter().map(|b| b.id).collect();

    assert_eq!(ids, vec![planned.batch.id, dated.batch.id]);
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nhello";
const JPEG: &[u8] = b"\xff\xd8\xffjpg";
const WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBPvp8";

async fn a_batch(db: &Db) -> i64 {
    let (r, ..) = pancakes(db).await;
    db.start_batch(&common::scope(db).await, r.id, 1.0).await.unwrap().batch.id
}

#[tokio::test]
async fn add_batch_photo_stores_the_file_by_sha256_and_lists_it() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();

    let img = db.add_batch_photo(&common::scope(&db).await, id, dir.path(), PNG).await.unwrap();

    assert_eq!(img.hash, "0e8754cfeb908584f7a2f2ccd08e37fda8d3d540cbaf2e16ac559c7ea59a7151");
    assert_eq!(img.mime, "image/png");
    assert_eq!(std::fs::read(dir.path().join(format!("{}.png", img.hash))).unwrap(), PNG);
    assert_eq!(db.get_batch(&common::scope(&db).await, id).await.unwrap().photos, vec![img]);
}

#[tokio::test]
async fn add_batch_photo_accepts_jpeg_and_webp_by_magic_bytes() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();

    let j = db.add_batch_photo(&common::scope(&db).await, id, dir.path(), JPEG).await.unwrap();
    let w = db.add_batch_photo(&common::scope(&db).await, id, dir.path(), WEBP).await.unwrap();

    assert_eq!((j.mime.as_str(), j.hash.as_str()), ("image/jpeg", "ffe44c37b0cbca06b59be3a612c30edafd3a420b0cd8339a626069ec5c3dcda8"));
    assert_eq!((w.mime.as_str(), w.hash.as_str()), ("image/webp", "dbbf62bee764399bcd586c2f2ed25d03fbfe8ccfa54f5a85022bba4366e8cadd"));
    assert!(dir.path().join(format!("{}.jpg", j.hash)).exists());
    assert!(dir.path().join(format!("{}.webp", w.hash)).exists());
}

#[tokio::test]
async fn add_batch_photo_rejects_other_content_and_writes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    for bytes in [&b"GIF89a...."[..], b"<html>", b""] {
        let err = db.add_batch_photo(&common::scope(&db).await, id, dir.path(), bytes).await.unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert!(db.get_batch(&common::scope(&db).await, id).await.unwrap().photos.is_empty());
}

#[tokio::test]
async fn add_batch_photo_rejects_more_than_10_mb() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let mut big = PNG.to_vec();
    big.resize(10 * 1024 * 1024, 0);
    db.add_batch_photo(&common::scope(&db).await, id, dir.path(), &big).await.unwrap();
    big.push(0);
    assert!(matches!(db.add_batch_photo(&common::scope(&db).await, id, dir.path(), &big).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn a_batch_holds_at_most_5_photos_and_the_same_photo_counts_once() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    db.add_batch_photo(&common::scope(&db).await, id, dir.path(), PNG).await.unwrap();
    db.add_batch_photo(&common::scope(&db).await, id, dir.path(), PNG).await.unwrap();
    assert_eq!(db.get_batch(&common::scope(&db).await, id).await.unwrap().photos.len(), 1);
    for n in 0..4u8 {
        let mut p = JPEG.to_vec();
        p.push(n);
        db.add_batch_photo(&common::scope(&db).await, id, dir.path(), &p).await.unwrap();
    }
    assert_eq!(db.get_batch(&common::scope(&db).await, id).await.unwrap().photos.len(), 5);

    let mut sixth = JPEG.to_vec();
    sixth.push(99);
    assert!(matches!(db.add_batch_photo(&common::scope(&db).await, id, dir.path(), &sixth).await, Err(Error::Validation(_))));
    assert_eq!(db.get_batch(&common::scope(&db).await, id).await.unwrap().photos.len(), 5);
}

#[tokio::test]
async fn add_batch_photo_to_a_missing_batch_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(db.add_batch_photo(&common::scope(&db).await, 999, dir.path(), PNG).await, Err(Error::NotFound("batch"))));
}
