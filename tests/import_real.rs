//! Imports the owner's real Mealie backup. Private data, so it lives outside
//! the repo and the test is ignored by default: `cargo test -- --ignored`.
use std::io::Read;
use std::path::PathBuf;

use mealier::{Db, import_mealie_backup};

fn real_backup() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/share"));
    data.join("_r_/mealier/fixtures/mealie_v3.28.0_2026.11.06.12.23.24.zip")
}

#[tokio::test]
#[ignore = "needs the private real backup"]
async fn the_real_backup_imports_with_matching_counts() {
    let path = real_backup();
    if !path.exists() {
        eprintln!("skipping: {} is missing", path.display());
        return;
    }
    let mut json = String::new();
    zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap().by_name("database.json").unwrap().read_to_string(&mut json).unwrap();
    let want = |table: &str| serde_json::from_str::<serde_json::Value>(&json).unwrap()[table].as_array().unwrap().len();

    let db = Db::open_memory().await.unwrap();
    let images = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, &path, images.path()).await.unwrap();

    assert_eq!((report.recipes, report.foods, report.units), (want("recipes"), want("ingredient_foods"), want("ingredient_units")));
    let rows = |t: &'static str| {
        let db = db.clone();
        async move { sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {t}")).fetch_one(db.pool()).await.unwrap() as usize }
    };
    assert_eq!(rows("recipes").await, want("recipes"));
    assert_eq!(rows("foods").await, want("ingredient_foods"));
    assert_eq!(rows("units").await, want("ingredient_units"));
    assert_eq!(rows("groups").await, want("groups"));
    assert_eq!(rows("households").await, want("households"));
    assert!(!report.skipped.iter().any(|l| l.contains("unknown table")), "{:?}", report.skipped);
    for t in ["long_live_tokens", "ai_providers"] {
        assert!(report.skipped.iter().any(|l| l.contains(t) && l.contains("skipped on purpose")), "no line for {t}: {:?}", report.skipped);
    }
    let text = format!("{report:?}");
    assert!(!text.contains("sk-"), "the report carries no key");
}
