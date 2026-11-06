//! The one image path (spec, Architecture): sniff, decode, bound, resize, re-encode, address by the hash of what is stored.
mod common;
use mealier::{Db, Error, Image};
use sha2::{Digest, Sha256};

async fn a_batch(db: &Db) -> i64 {
    let sc = common::scope(db).await;
    let r = db.create_recipe(&sc, mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    db.start_batch(&sc, r.id, 1.0).await.unwrap().batch.id
}

/// Stores `bytes` as a photo of a fresh batch and returns the row and the file's bytes.
async fn stored(bytes: &[u8]) -> (Image, Vec<u8>) {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let img = db.add_batch_photo(&common::scope(&db).await, id, dir.path(), bytes).await.unwrap();
    let file = std::fs::read(dir.path().join(img.file_name())).unwrap();
    (img, file)
}

#[tokio::test]
async fn a_large_png_is_resized_so_its_longest_side_is_2048() {
    let (img, file) = stored(&common::png(3000, 1500, 1)).await;
    assert_eq!(common::dims(&file), (2048, 1024));
    assert_eq!(img.mime, "image/png");
}

#[tokio::test]
async fn a_tall_jpeg_keeps_its_aspect_and_its_format() {
    let (img, file) = stored(&common::jpeg(1500, 4000, 1)).await;
    assert_eq!(common::dims(&file), (768, 2048));
    assert_eq!(img.mime, "image/jpeg");
    assert!(img.file_name().ends_with(".jpg"));
}

#[tokio::test]
async fn a_large_webp_is_resized_and_stays_webp() {
    let (img, file) = stored(&common::webp(4096, 2048, 1)).await;
    assert_eq!(common::dims(&file), (2048, 1024));
    assert_eq!(img.mime, "image/webp");
}

#[tokio::test]
async fn a_small_image_keeps_its_size() {
    let (_, file) = stored(&common::png(100, 50, 1)).await;
    assert_eq!(common::dims(&file), (100, 50));
}

#[tokio::test]
async fn the_hash_is_the_sha256_of_the_stored_bytes() {
    let (img, file) = stored(&common::jpeg(3000, 100, 7)).await;
    let digest: String = Sha256::digest(&file).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(img.hash, digest);
}

#[tokio::test]
async fn exif_orientation_is_applied_before_the_metadata_is_dropped() {
    use image::ImageEncoder;
    // A minimal EXIF block (little endian, one IFD entry): Orientation = 6, "rotate 90 clockwise".
    let exif = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0];
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new(&mut out);
    enc.set_exif_metadata(exif).unwrap();
    enc.write_image(&[200u8; 4 * 2 * 3], 4, 2, image::ExtendedColorType::Rgb8).unwrap();
    assert_eq!(common::dims(&out), (4, 2), "the file as shot");

    let (_, file) = stored(&out).await;
    assert_eq!(common::dims(&file), (2, 4), "stored upright");
}

#[tokio::test]
async fn garbage_that_starts_like_a_png_is_refused_and_nothing_is_written() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let sc = common::scope(&db).await;
    let mut truncated = common::png(64, 64, 1);
    truncated.truncate(40);
    for bytes in [b"\x89PNG\r\n\x1a\nhello".to_vec(), truncated, b"\xff\xd8\xffnot a jpeg".to_vec(), b"RIFF\0\0\0\0WEBPjunk".to_vec()] {
        let err = db.add_batch_photo(&sc, id, dir.path(), &bytes).await.unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM images").fetch_one(db.pool()).await.unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn a_gif_is_refused_even_when_it_is_a_valid_one() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    // 1x1 GIF89a
    let gif = [b"GIF89a".as_slice(), &[1, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 255, 255, 255, 0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 1, 0, 0x3b]].concat();
    let err = db.add_batch_photo(&common::scope(&db).await, id, dir.path(), &gif).await.unwrap_err();
    assert!(matches!(err, Error::Validation(_)), "{err:?}");
}

#[tokio::test]
async fn an_image_wider_than_12000_pixels_is_refused_before_it_is_decoded() {
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let sc = common::scope(&db).await;
    for bytes in [common::png(12001, 1, 1), common::png(1, 12001, 1)] {
        let err = db.add_batch_photo(&sc, id, dir.path(), &bytes).await.unwrap_err();
        assert!(matches!(&err, Error::Validation(m) if m.contains("12000")), "{err:?}");
    }
    db.add_batch_photo(&sc, id, dir.path(), &common::png(12000, 1, 1)).await.unwrap();
}

#[tokio::test]
async fn the_same_bytes_are_one_row_and_one_file_even_for_two_batches() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let (a, b) = (a_batch(&db).await, a_batch(&db).await);
    let dir = tempfile::tempdir().unwrap();
    let bytes = common::png(300, 200, 5);
    let first = db.add_batch_photo(&sc, a, dir.path(), &bytes).await.unwrap();
    let second = db.add_batch_photo(&sc, b, dir.path(), &bytes).await.unwrap();
    assert_eq!(first, second);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM images").fetch_one(db.pool()).await.unwrap();
    assert_eq!((rows, std::fs::read_dir(dir.path()).unwrap().count()), (1, 1));
}

#[tokio::test]
async fn the_same_picture_saved_two_ways_is_one_image() {
    use image::ImageEncoder;
    let best = common::png(80, 60, 9);
    let mut fast = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(&mut fast, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::NoFilter)
        .write_image(image::RgbImage::from_pixel(80, 60, image::Rgb([9, 40, 90])).as_raw(), 80, 60, image::ExtendedColorType::Rgb8)
        .unwrap();
    assert_ne!(fast, best, "two different files, one picture");
    let db = Db::open_memory().await.unwrap();
    let id = a_batch(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let sc = common::scope(&db).await;
    let a = db.add_batch_photo(&sc, id, dir.path(), &fast).await.unwrap();
    let b = db.add_batch_photo(&sc, id, dir.path(), &best).await.unwrap();
    assert_eq!(a.hash, b.hash);
}

// ---------- snapshots remember which images a recipe had ----------

async fn recipe(db: &Db) -> i64 {
    db.create_recipe(&common::scope(db).await, mealier::RecipeInput { name: "Soup".into(), steps: vec!["Stir.".into()], ..Default::default() }).await.unwrap().id
}

#[tokio::test]
async fn replacing_a_recipes_image_leaves_the_old_snapshot_listing_the_old_hash() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let id = recipe(&db).await;
    let dir = tempfile::tempdir().unwrap();

    let first = db.set_recipe_image(&sc, id, dir.path(), &common::png(40, 30, 1)).await.unwrap();
    let second = db.set_recipe_image(&sc, id, dir.path(), &common::png(40, 30, 2)).await.unwrap();

    let (old, new) = (first.content.images.clone(), second.content.images.clone());
    assert_eq!((old.len(), new.len()), (1, 1));
    assert_ne!(old, new);
    assert_eq!(db.snapshot(&sc, first.current_snapshot_id).await.unwrap().content.images, old, "history is not rewritten");
    assert_eq!(db.get_recipe(&sc, id).await.unwrap().unwrap().content.images, new);
    // the live recipe has the new image only; the old file is still there for the old snapshot
    let live: Vec<String> = db.recipe_images(&sc, id).await.unwrap().into_iter().map(|i| i.hash).collect();
    assert_eq!(live, new);
    assert!(dir.path().join(format!("{}.png", old[0])).exists());
}

#[tokio::test]
async fn editing_a_recipe_keeps_its_image_in_the_new_snapshot() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let id = recipe(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let with = db.set_recipe_image(&sc, id, dir.path(), &common::png(10, 10, 3)).await.unwrap();
    let edited = db.update_recipe(&sc, id, mealier::RecipeInput { name: "Soup 2".into(), ..Default::default() }).await.unwrap();
    assert_eq!(edited.content.images, with.content.images);
    assert!(edited.content.assets.is_empty());
}

#[tokio::test]
async fn setting_an_image_on_a_recipe_of_another_group_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let id = db.create_recipe(&t.a, mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap().id;
    let dir = tempfile::tempdir().unwrap();
    let err = db.set_recipe_image(&t.c, id, dir.path(), &common::png(10, 10, 3)).await.unwrap_err();
    assert!(matches!(err, Error::NotFound("recipe")), "{err:?}");
    assert!(db.recipe_images(&t.a, id).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_snapshot_written_before_images_were_recorded_still_reads() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let id = recipe(&db).await;
    let cur = db.get_recipe(&sc, id).await.unwrap().unwrap();
    let mut v = serde_json::to_value(&cur.content).unwrap();
    v.as_object_mut().unwrap().remove("images");
    v.as_object_mut().unwrap().remove("assets");
    sqlx::query("INSERT INTO recipe_snapshots (recipe_id, content) VALUES (?, ?)").bind(id).bind(v.to_string()).execute(db.pool()).await.unwrap();
    let last: i64 = sqlx::query_scalar("SELECT max(id) FROM recipe_snapshots").fetch_one(db.pool()).await.unwrap();
    let old = db.snapshot(&sc, last).await.unwrap();
    assert!(old.content.images.is_empty() && old.content.assets.is_empty());
}
