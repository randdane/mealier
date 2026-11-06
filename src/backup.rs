//! Export and restore (spec, "Backup and export").
//!
//! Export: `VACUUM INTO` a staging file under the data directory (a read transaction, so writers
//! carry on), then zip that copy and every image it lists. Images never change once written and
//! nothing deletes them while an export runs, so the pair is consistent. Restore: unpack to a
//! staging directory, check everything, and only then move the current data aside and the staged
//! data in. Anything that goes wrong before the swap leaves the current data as it was.
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use askama::Template;
use askama_web::WebTemplate;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Connection, SqliteConnection, SqlitePool};
use tokio::sync::Notify;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::api::{AppState, ErrorBody};
use crate::{Admin, Error, Result};

pub const DB_FILE: &str = "mealier.db";
const IMAGES: &str = "images";
const EXPORT_STAGING: &str = "export-staging";
const RESTORE_STAGING: &str = "restore-staging";
const LOCK_FILE: &str = "mealier.lock";
/// What a data directory holds that a restore replaces. Everything else in it is left alone.
const OWNED: [&str; 4] = [DB_FILE, "mealier.db-wal", "mealier.db-shm", IMAGES];

fn bad(m: impl Into<String>) -> Error {
    Error::Validation(m.into())
}

fn unique() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// `YYYYMMDDTHHMMSSZ`, UTC, for names of things kept.
fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Removes a path when dropped, whatever happened.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// ---------- the data directory lock ----------

/// Held by the running server for as long as it runs. The offline restore takes the same lock, so it
/// cannot swap files under a live server.
pub struct DataLock(#[allow(dead_code)] File);

pub fn lock_data_dir(dir: &Path) -> Result<DataLock> {
    fs::create_dir_all(dir)?;
    let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK_FILE))?;
    // SAFETY: flock on a descriptor we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::WouldBlock { bad("another mealier is using this data directory; stop it first") } else { e.into() });
    }
    Ok(DataLock(f))
}

// ---------- export ----------

/// Writes the zip to `out` (whole or not at all).
pub async fn export(pool: &SqlitePool, data_dir: &Path, out: &Path) -> Result<()> {
    let staging = data_dir.join(EXPORT_STAGING);
    fs::create_dir_all(&staging)?;
    let copy = staging.join(format!("{}.db", unique()));
    let _copy = Cleanup(copy.clone());
    // A read transaction: writers are not held up while the copy is made, nor during the zipping below.
    sqlx::query("VACUUM INTO ?").bind(copy.to_str().ok_or_else(|| bad("data directory path is not text"))?).execute(pool).await?;

    let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&copy).read_only(true)).await?;
    let images: Vec<(String, String)> = sqlx::query_as("SELECT hash, mime FROM images ORDER BY hash").fetch_all(&mut conn).await?;
    conn.close().await?;

    let (images_dir, out, partial) = (data_dir.join(IMAGES), out.to_path_buf(), out.with_extension("partial"));
    let _partial = Cleanup(partial.clone());
    tokio::task::spawn_blocking(move || {
        write_zip(&copy, &images, &images_dir, &partial)?;
        fs::rename(&partial, &out)?;
        Ok::<_, Error>(())
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
}

fn write_zip(db_copy: &Path, images: &[(String, String)], images_dir: &Path, out: &Path) -> Result<()> {
    let zerr = |e: zip::result::ZipError| Error::Internal(e.to_string());
    let mut zip = ZipWriter::new(File::create(out)?);
    zip.start_file(DB_FILE, SimpleFileOptions::default().compression_method(CompressionMethod::Deflated).large_file(true)).map_err(zerr)?;
    io::copy(&mut File::open(db_copy)?, &mut zip)?;
    // Images are already compressed.
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored).large_file(true);
    for (hash, mime) in images {
        let ext = crate::images::ext(mime).ok_or_else(|| bad(format!("image {hash} has a type ({mime}) that cannot be exported")))?;
        let name = format!("{hash}.{ext}");
        let mut file = File::open(images_dir.join(&name)).map_err(|e| bad(format!("image file {name} cannot be read ({}); the library has a hole", e.kind())))?;
        zip.start_file(format!("{IMAGES}/{name}"), stored).map_err(zerr)?;
        let (mut digest, mut buf) = (Sha256::new(), [0u8; 64 * 1024]);
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
            zip.write_all(&buf[..n])?;
        }
        if hex(&digest.finalize()) != *hash {
            return Err(bad(format!("image file {name} does not match its sha256; the library is damaged")));
        }
    }
    zip.finish().map_err(zerr)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The CLI's export: reads `<data_dir>/mealier.db` without migrating it or taking the lock, so it
/// works while the server runs.
pub async fn export_dir(data_dir: &Path, out: &Path) -> Result<()> {
    let file = data_dir.join(DB_FILE);
    if !file.exists() {
        return Err(bad(format!("no database at {}", file.display())));
    }
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(SqliteConnectOptions::new().filename(&file)).await?;
    let r = export(&pool, data_dir, out).await;
    pool.close().await;
    r
}

// ---------- restore ----------

#[derive(Debug)]
pub struct Restored {
    /// Where the data that was replaced went; `None` when there was none.
    pub previous: Option<PathBuf>,
}

/// The CLI's restore: the server must be stopped. It takes the data directory lock the server holds.
pub async fn restore_offline(zip: &Path, data_dir: &Path) -> Result<Restored> {
    let _lock = lock_data_dir(data_dir)?;
    restore(zip, data_dir).await
}

/// Checks `zip` and swaps it in. The caller makes sure nothing else writes the data directory.
pub async fn restore(zip: &Path, data_dir: &Path) -> Result<Restored> {
    fs::create_dir_all(data_dir)?;
    let staging = data_dir.join(RESTORE_STAGING);
    // A leftover of an earlier attempt that died.
    match fs::remove_dir_all(&staging) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    fs::create_dir_all(staging.join(IMAGES))?;
    let r = async {
        let (zip, to) = (zip.to_path_buf(), staging.clone());
        tokio::task::spawn_blocking(move || unpack(&zip, &to)).await.map_err(|e| Error::Internal(e.to_string()))??;
        validate(&staging).await?;
        swap(data_dir, &staging)
    }
    .await;
    let _ = fs::remove_dir_all(&staging);
    r
}

/// The only names an export writes: the database and `images/<64 hex>.<png|jpg|webp>`.
fn entry_path(name: &str) -> Option<PathBuf> {
    if name == DB_FILE {
        return Some(PathBuf::from(DB_FILE));
    }
    let (hash, ext) = name.strip_prefix("images/")?.split_once('.')?;
    let hash_ok = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (hash_ok && matches!(ext, "png" | "jpg" | "webp")).then(|| Path::new(IMAGES).join(format!("{hash}.{ext}")))
}

fn unpack(zip: &Path, staging: &Path) -> Result<()> {
    let zerr = |e: zip::result::ZipError| bad(format!("not a usable zip file: {e}"));
    let mut archive = ZipArchive::new(File::open(zip)?).map_err(zerr)?;
    let mut seen = HashSet::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zerr)?;
        let name = entry.name().map_err(zerr)?.to_string();
        if entry.is_dir() && name == "images/" {
            continue;
        }
        let Some(rel) = entry_path(&name).filter(|_| !entry.is_dir() && !entry.is_symlink()) else {
            return Err(bad(format!("the archive has an entry mealier never writes: {name:?}")));
        };
        if !seen.insert(name.clone()) {
            return Err(bad(format!("the archive has {name:?} twice")));
        }
        let size = entry.size();
        let mut out = fs::OpenOptions::new().write(true).create_new(true).open(staging.join(rel))?;
        // Bounded by the size the archive declares, so a lying header cannot fill the disk.
        io::copy(&mut (&mut entry).take(size), &mut out)?;
    }
    if !seen.contains(DB_FILE) {
        return Err(bad("the archive has no mealier.db"));
    }
    Ok(())
}

async fn validate(staging: &Path) -> Result<()> {
    let unusable = |e: sqlx::Error| bad(format!("mealier.db is not a database mealier can read: {e}"));
    let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(staging.join(DB_FILE)).read_only(true)).await.map_err(unusable)?;
    let verdict: String = sqlx::query_scalar("PRAGMA quick_check").fetch_one(&mut conn).await.map_err(unusable)?;
    if verdict != "ok" {
        return Err(bad(format!("mealier.db is damaged: {verdict}")));
    }
    let has: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'").fetch_one(&mut conn).await.map_err(unusable)?;
    if has == 0 {
        return Err(bad("mealier.db has no migrations table; it is not a mealier database"));
    }
    let applied: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations").fetch_all(&mut conn).await.map_err(unusable)?;
    let known = sqlx::migrate!();
    let newest = known.iter().map(|m| m.version).max().unwrap_or(0);
    for (version, success, checksum) in &applied {
        if *version > newest {
            return Err(bad(format!("the backup was made by a newer mealier (schema {version}; this build knows up to {newest})")));
        }
        if !success {
            return Err(bad(format!("migration {version} in the backup did not finish")));
        }
        if known.iter().any(|m| m.version == *version && m.checksum.as_ref() != checksum.as_slice()) {
            return Err(bad(format!("migration {version} in the backup differs from this build's")));
        }
    }
    let images: Vec<(String, String)> = sqlx::query_as("SELECT hash, mime FROM images").fetch_all(&mut conn).await.map_err(unusable)?;
    conn.close().await.map_err(unusable)?;

    let dir = staging.join(IMAGES);
    tokio::task::spawn_blocking(move || {
        for (hash, mime) in images {
            let ext = crate::images::ext(&mime).ok_or_else(|| bad(format!("image {hash} has a type ({mime}) mealier does not store")))?;
            let bytes = fs::read(dir.join(format!("{hash}.{ext}"))).map_err(|_| bad(format!("the archive lacks the image {hash}.{ext}")))?;
            if hex(&Sha256::digest(&bytes)) != hash {
                return Err(bad(format!("image {hash}.{ext} does not match its sha256")));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
}

/// Moves the current data into `pre-restore-<stamp>/`, then the staged data in. A failure half way
/// puts everything back.
fn swap(data_dir: &Path, staging: &Path) -> Result<Restored> {
    let present: Vec<&str> = OWNED.into_iter().filter(|n| data_dir.join(n).exists()).collect();
    let previous = (!present.is_empty()).then(|| data_dir.join(format!("pre-restore-{}", stamp())));
    if let Some(p) = &previous {
        fs::create_dir(p)?;
    }
    let (mut moved, mut placed) = (Vec::new(), Vec::new());
    let result = (|| -> io::Result<()> {
        for n in &present {
            match fs::rename(data_dir.join(n), previous.as_ref().expect("something is present").join(n)) {
                // SQLite deletes its -wal and -shm itself when the last connection closes.
                Err(e) if e.kind() == io::ErrorKind::NotFound && n.starts_with("mealier.db-") => {}
                other => other?,
            }
            moved.push(*n);
        }
        for n in [DB_FILE, IMAGES] {
            fs::rename(staging.join(n), data_dir.join(n))?;
            placed.push(n);
        }
        Ok(())
    })();
    if let Err(e) = result {
        for n in placed {
            let p = data_dir.join(n);
            let _ = if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) };
        }
        for n in moved {
            let _ = fs::rename(previous.as_ref().expect("something moved").join(n), data_dir.join(n));
        }
        if let Some(p) = &previous {
            let _ = fs::remove_dir(p);
        }
        return Err(e.into());
    }
    Ok(Restored { previous })
}

// ---------- the running server ----------

/// A restore from the UI or API swaps the files under a pool that still has the old ones open, so the
/// process must exit afterwards. `request` marks that; `main` shuts down on it (after the response is
/// sent), the container restarts, and until then every request but the health check is refused.
pub struct Restart {
    requested: AtomicBool,
    busy: AtomicBool,
    notify: Notify,
}

impl Default for Restart {
    fn default() -> Self {
        Self::new()
    }
}

impl Restart {
    pub fn new() -> Restart {
        Restart { requested: AtomicBool::new(false), busy: AtomicBool::new(false), notify: Notify::new() }
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Resolves once a restore has been swapped in.
    pub async fn requested(&self) {
        self.notify.notified().await;
    }

    fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

/// One restore at a time, and none after one has been swapped in.
struct Busy<'a>(&'a Restart);

impl<'a> Busy<'a> {
    fn take(r: &'a Restart) -> Result<Busy<'a>> {
        if r.is_requested() || r.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return Err(bad("a restore is already under way"));
        }
        Ok(Busy(r))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

impl AppState {
    /// The data directory: the parent of `images`. Export and restore refuse any other layout, so they
    /// cannot be pointed at a stray directory.
    fn data_dir(&self) -> Result<&Path> {
        match (self.images_dir.file_name(), self.images_dir.parent()) {
            (Some(n), Some(parent)) if n == IMAGES => Ok(parent),
            _ => Err(bad("export and restore need the images directory to be <data dir>/images")),
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    let upload = || DefaultBodyLimit::disable();
    Router::new()
        .route("/api/export", get(download))
        .route("/api/restore", post(restore_api).layer(upload()))
        .route("/settings/backup", get(backup_page))
        .route("/settings/backup/restore", post(restore_form).layer(upload()))
}

#[utoipa::path(
    get, path = "/api/export",
    responses(
        (status = 200, description = "A zip of the database (`mealier.db`) and every image (`images/<sha256>.<ext>`)", content_type = "application/zip", body = Vec<u8>),
        (status = 403, description = "Not an admin", body = ErrorBody),
        (status = 422, description = "An image file is missing or damaged, so the library cannot be exported whole", body = ErrorBody),
    )
)]
/// The export as a download. The zip is built in the staging directory, opened, and unlinked at once:
/// the open handle keeps it alive until the download ends, and nothing is left behind if it breaks off.
pub(crate) async fn download(_: Admin, State(s): State<AppState>) -> Result<Response> {
    let data_dir = s.data_dir()?;
    let zip = data_dir.join(EXPORT_STAGING).join(format!("{}.zip", unique()));
    fs::create_dir_all(data_dir.join(EXPORT_STAGING))?;
    let _zip = Cleanup(zip.clone());
    export(s.db.pool(), data_dir, &zip).await?;
    let file = tokio::fs::File::open(&zip).await?;
    let len = file.metadata().await?.len();
    drop(_zip);
    let headers = [
        (header::CONTENT_TYPE, "application/zip".to_string()),
        (header::CONTENT_DISPOSITION, format!("attachment; filename=\"mealier-export-{}.zip\"", &stamp()[..8])),
        (header::CONTENT_LENGTH, len.to_string()),
    ];
    Ok((headers, Body::from_stream(tokio_util::io::ReaderStream::new(file))).into_response())
}

/// Streams the uploaded `file` field to disk (never into memory), restores from it and asks to be restarted.
async fn accept_upload(s: &AppState, restart: &Restart, mut form: Multipart) -> Result<Restored> {
    let data_dir = s.data_dir()?;
    let _busy = Busy::take(restart)?;
    let upload = data_dir.join("restore.upload");
    let _upload = Cleanup(upload.clone());
    let refused = |e: axum::extract::multipart::MultipartError| bad(format!("bad upload: {e}"));
    let mut got = false;
    while let Some(mut field) = form.next_field().await.map_err(refused)? {
        if field.name() == Some("file") {
            let mut out = tokio::fs::File::create(&upload).await?;
            while let Some(chunk) = field.chunk().await.map_err(refused)? {
                tokio::io::AsyncWriteExt::write_all(&mut out, &chunk).await?;
            }
            tokio::io::AsyncWriteExt::flush(&mut out).await?;
            got = true;
            break;
        }
    }
    if !got {
        return Err(bad("form field `file` is missing"));
    }
    let restored = restore(&upload, data_dir).await?;
    restart.request();
    Ok(restored)
}

fn previous_name(r: &Restored) -> Option<String> {
    r.previous.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
}

#[utoipa::path(
    post, path = "/api/restore",
    request_body(content_type = "multipart/form-data", content = String, description = "An export zip in the form field `file`"),
    responses(
        (status = 202, description = "Checked and swapped in; the old data is kept in `previous` (a directory name in the data directory). The server exits right after answering and every request gets 503 until it is back", body = Object),
        (status = 403, description = "Not an admin", body = ErrorBody),
        (status = 422, description = "The zip was refused (unsafe entry name, damaged or newer database, missing or mismatched image); nothing changed", body = ErrorBody),
    )
)]
pub(crate) async fn restore_api(_: Admin, State(s): State<AppState>, Extension(restart): Extension<Arc<Restart>>, form: Multipart) -> Result<Response> {
    let restored = accept_upload(&s, &restart, form).await?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "restarting": true, "previous": previous_name(&restored) }))).into_response())
}

#[derive(Template, WebTemplate)]
#[template(path = "backup.html")]
struct BackupPage {
    restarting: bool,
    previous: String,
}

async fn backup_page(_: Admin) -> BackupPage {
    BackupPage { restarting: false, previous: String::new() }
}

async fn restore_form(_: Admin, State(s): State<AppState>, Extension(restart): Extension<Arc<Restart>>, form: Multipart) -> Result<Response> {
    let restored = accept_upload(&s, &restart, form).await?;
    Ok((StatusCode::ACCEPTED, BackupPage { restarting: true, previous: previous_name(&restored).unwrap_or_default() }).into_response())
}
