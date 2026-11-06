// Rebuild when a migration is added; sqlx::migrate! does not track new files.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
