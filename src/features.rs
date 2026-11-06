//! Step 14, part A, beyond the recipe's own content: assets, comments, extras on foods and shopping
//! lists, and food substitutions.
use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use sqlx::Row;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};

use crate::api::{AppState, ErrorBody};
use crate::recipes::{check_extras, rewrite};
use crate::{Comment, Db, Error, Result, Scope};

/// A file attached to a recipe. The bytes are stored as uploaded (never decoded) under their sha256.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Asset {
    pub hash: String,
    pub name: String,
    pub icon: String,
    /// The uploaded name, cleaned: it is what a download is saved as.
    pub file_name: String,
    pub size: i64,
}

/// Biggest asset accepted.
pub const MAX_ASSET_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct FoodSubstitution {
    /// Names this entry in `/api/food-substitutions/{id}`.
    pub id: i64,
    /// The food that can stand in.
    pub food_id: i64,
    pub name: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct FoodDetail {
    pub id: i64,
    pub name: String,
    pub aliases: Vec<String>,
    pub label_id: Option<i64>,
    pub extras: BTreeMap<String, String>,
    pub substitutions: Vec<FoodSubstitution>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(default)]
pub struct SubstitutionInput {
    pub food_id: i64,
    pub note: String,
}

/// The file name a download is saved under: no directories, ASCII letters, digits, space and `._-()`
/// only, no leading dot, at most 100 characters.
pub fn clean_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || " ._-()".contains(c) { c } else { '_' }).collect();
    let cleaned: String = cleaned.trim_start_matches('.').trim().chars().take(100).collect();
    if cleaned.is_empty() { "file".into() } else { cleaned }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn asset_from(r: &sqlx::sqlite::SqliteRow) -> Asset {
    Asset { hash: r.get(0), name: r.get(1), icon: r.get(2), file_name: r.get(3), size: r.get(4) }
}

const ASSET_COLS: &str = "a.hash, ra.name, ra.icon, ra.file_name, a.size";

fn extras_of(json: &str) -> Result<BTreeMap<String, String>> {
    Ok(serde_json::from_str(json)?)
}

impl Db {
    /// Stores the file and attaches it. An edit like any other: it writes a snapshot, so a locked recipe refuses it.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_recipe_asset(&self, sc: &Scope, recipe_id: i64, dir: &Path, file_name: &str, name: &str, icon: &str, bytes: &[u8]) -> Result<Asset> {
        self.check_recipe(sc, recipe_id).await?;
        if bytes.is_empty() {
            return Err(Error::Validation("the file is empty".into()));
        }
        if bytes.len() > MAX_ASSET_BYTES {
            return Err(Error::Validation("an asset can be at most 20 MB".into()));
        }
        let hash = hex(&Sha256::digest(bytes));
        let file_name = clean_file_name(file_name);
        let name = match name.trim() {
            "" => file_name.clone(),
            n => n.chars().take(200).collect(),
        };
        let icon: String = icon.trim().chars().take(100).collect();
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO assets (hash, size) VALUES (?, ?)").bind(&hash).bind(bytes.len() as i64).execute(&mut *tx).await?;
        let new = sqlx::query("INSERT OR IGNORE INTO recipe_assets (recipe_id, hash, name, icon, file_name) VALUES (?, ?, ?, ?, ?)")
            .bind(recipe_id)
            .bind(&hash)
            .bind(&name)
            .bind(&icon)
            .bind(&file_name)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if new > 0 {
            rewrite(&mut tx, sc.group_id, recipe_id, None, None).await?;
            // Whole or not at all, like images: a half-written file would sit under the hash of the complete one.
            std::fs::create_dir_all(dir)?;
            let tmp = dir.join(format!("{hash}.tmp"));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(tmp, dir.join(&hash))?;
        }
        tx.commit().await?;
        self.recipe_asset(sc, recipe_id, &hash).await
    }

    pub async fn recipe_assets(&self, sc: &Scope, recipe_id: i64) -> Result<Vec<Asset>> {
        self.check_recipe(sc, recipe_id).await?;
        let rows = sqlx::query(&format!("SELECT {ASSET_COLS} FROM recipe_assets ra JOIN assets a ON a.hash = ra.hash WHERE ra.recipe_id = ? ORDER BY ra.id")).bind(recipe_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(asset_from).collect())
    }

    /// One of the recipe's current assets; `NotFound` unless the recipe is the caller's group's and has it.
    pub async fn recipe_asset(&self, sc: &Scope, recipe_id: i64, hash: &str) -> Result<Asset> {
        self.check_recipe(sc, recipe_id).await?;
        let row = sqlx::query(&format!("SELECT {ASSET_COLS} FROM recipe_assets ra JOIN assets a ON a.hash = ra.hash WHERE ra.recipe_id = ? AND ra.hash = ?")).bind(recipe_id).bind(hash).fetch_optional(&self.pool).await?;
        row.as_ref().map(asset_from).ok_or(Error::NotFound("asset"))
    }

    /// Detaches it (a new snapshot without the hash; older snapshots still list it). The file stays.
    pub async fn remove_recipe_asset(&self, sc: &Scope, recipe_id: i64, hash: &str) -> Result<()> {
        self.check_recipe(sc, recipe_id).await?;
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query("DELETE FROM recipe_assets WHERE recipe_id = ? AND hash = ?").bind(recipe_id).bind(hash).execute(&mut *tx).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("asset"));
        }
        rewrite(&mut tx, sc.group_id, recipe_id, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Refused when the recipe disables comments.
    pub async fn add_comment(&self, sc: &Scope, recipe_id: i64, text: &str) -> Result<Comment> {
        let recipe = self.get_recipe(sc, recipe_id).await?.ok_or(Error::NotFound("recipe"))?;
        if recipe.content.settings.disable_comments {
            return Err(Error::Validation("comments are disabled for this recipe".into()));
        }
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Validation("a comment needs text".into()));
        }
        let user = Some(sc.user_id).filter(|u| *u > 0);
        let row = sqlx::query("INSERT INTO recipe_comments (recipe_id, user_id, text, created_at) VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) RETURNING user_id, text, created_at")
            .bind(recipe_id)
            .bind(user)
            .bind(text)
            .fetch_one(&self.pool)
            .await?;
        Ok(Comment { user_id: row.get(0), text: row.get(1), created_at: row.get(2) })
    }

    pub async fn food(&self, sc: &Scope, id: i64) -> Result<FoodDetail> {
        let r = sqlx::query("SELECT id, name, aliases, label_id, extras FROM foods WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?.ok_or(Error::NotFound("food"))?;
        let aliases: String = r.get(2);
        Ok(FoodDetail {
            id,
            name: r.get(1),
            aliases: serde_json::from_str(&aliases)?,
            label_id: r.get(3),
            extras: extras_of(r.get(4))?,
            substitutions: self.food_substitutions(sc, id).await?,
        })
    }

    async fn food_substitutions(&self, sc: &Scope, food_id: i64) -> Result<Vec<FoodSubstitution>> {
        let rows = sqlx::query("SELECT s.id, f.id, f.name, s.note FROM food_substitutions s JOIN foods f ON f.id = s.substitute_food_id WHERE s.food_id = ? AND f.group_id = ? ORDER BY f.name, f.id")
            .bind(food_id)
            .bind(sc.group_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| FoodSubstitution { id: r.get(0), food_id: r.get(1), name: r.get(2), note: r.get(3) }).collect())
    }

    async fn food_in_group(&self, sc: &Scope, id: i64) -> Result<()> {
        sqlx::query("SELECT 1 FROM foods WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?.map(|_| ()).ok_or(Error::NotFound("food"))
    }

    pub async fn set_food_extras(&self, sc: &Scope, id: i64, extras: BTreeMap<String, String>) -> Result<()> {
        let extras = check_extras(extras)?;
        let n = sqlx::query("UPDATE foods SET extras = ? WHERE id = ? AND group_id = ?").bind(serde_json::to_string(&extras)?).bind(id).bind(sc.group_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("food"));
        }
        Ok(())
    }

    pub async fn add_food_substitution(&self, sc: &Scope, food_id: i64, input: SubstitutionInput) -> Result<Vec<FoodSubstitution>> {
        self.food_in_group(sc, food_id).await?;
        self.food_in_group(sc, input.food_id).await?;
        if input.food_id == food_id {
            return Err(Error::Validation("a food cannot substitute itself".into()));
        }
        // Adding a food that is already there changes its note.
        sqlx::query("INSERT INTO food_substitutions (food_id, substitute_food_id, note) VALUES (?, ?, ?) ON CONFLICT (food_id, substitute_food_id) DO UPDATE SET note = excluded.note")
            .bind(food_id)
            .bind(input.food_id)
            .bind(input.note.trim())
            .execute(&self.pool)
            .await?;
        self.food_substitutions(sc, food_id).await
    }

    /// The food an entry belongs to, if the entry is in the caller's group.
    async fn substitution_owner(&self, sc: &Scope, id: i64) -> Result<i64> {
        sqlx::query_scalar("SELECT s.food_id FROM food_substitutions s JOIN foods f ON f.id = s.food_id WHERE s.id = ? AND f.group_id = ?")
            .bind(id)
            .bind(sc.group_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("substitution"))
    }

    pub async fn update_food_substitution(&self, sc: &Scope, id: i64, note: &str) -> Result<Vec<FoodSubstitution>> {
        let food = self.substitution_owner(sc, id).await?;
        sqlx::query("UPDATE food_substitutions SET note = ? WHERE id = ?").bind(note.trim()).bind(id).execute(&self.pool).await?;
        self.food_substitutions(sc, food).await
    }

    pub async fn remove_food_substitution(&self, sc: &Scope, id: i64) -> Result<()> {
        self.substitution_owner(sc, id).await?;
        sqlx::query("DELETE FROM food_substitutions WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn set_list_extras(&self, sc: &Scope, list_id: i64, extras: BTreeMap<String, String>) -> Result<()> {
        let extras = check_extras(extras)?;
        let n = sqlx::query("UPDATE shopping_lists SET extras = ? WHERE id = ? AND household_id = ?").bind(serde_json::to_string(&extras)?).bind(list_id).bind(sc.household_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("shopping list"));
        }
        Ok(())
    }

    /// The substitutes of each food in `ids`, by food id (for a page that shows "or ..." beside a line).
    pub(crate) async fn substitutes_of(&self, sc: &Scope, ids: &[i64]) -> Result<std::collections::HashMap<i64, Vec<FoodSubstitution>>> {
        let mut out = std::collections::HashMap::new();
        for id in ids {
            let subs = self.food_substitutions(sc, *id).await?;
            if !subs.is_empty() {
                out.insert(*id, subs);
            }
        }
        Ok(out)
    }
}

// ---------- API ----------

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // The cap is checked by the store with a message; this one only stops a runaway body.
        .route("/api/recipes/{id}/assets", get(list_assets).post(upload_asset).layer(DefaultBodyLimit::max(MAX_ASSET_BYTES + 1024 * 1024)))
        .route("/api/assets/{id}", get(download_asset).delete(delete_asset))
        .route("/api/recipes/{id}/comments", get(list_comments).post(post_comment))
        .route("/api/foods/{id}", get(get_food))
        .route("/api/foods/{id}/extras", put(put_food_extras))
        .route("/api/foods/{id}/substitutions", get(list_food_substitutions).post(add_food_substitution_route))
        .route("/api/food-substitutions/{id}", put(put_food_substitution).delete(delete_food_substitution))
        .route("/api/shopping-lists/{id}/extras", put(put_list_extras))
}

/// The fields of an asset upload: `file` (required), `name` and `icon`.
pub(crate) struct Upload {
    pub file_name: String,
    pub bytes: axum::body::Bytes,
    pub name: String,
    pub icon: String,
}

pub(crate) async fn read_upload(mut form: Multipart) -> Result<Upload> {
    let bad = |e: axum::extract::multipart::MultipartError| Error::Validation(format!("bad upload: {e}"));
    let (mut file, mut name, mut icon) = (None, String::new(), String::new());
    while let Some(field) = form.next_field().await.map_err(bad)? {
        match field.name() {
            Some("file") => {
                let file_name = field.file_name().unwrap_or_default().to_string();
                file = Some((file_name, field.bytes().await.map_err(bad)?));
            }
            Some("name") => name = field.text().await.map_err(bad)?,
            Some("icon") => icon = field.text().await.map_err(bad)?,
            _ => {}
        }
    }
    let (file_name, bytes) = file.ok_or_else(|| Error::Validation("form field `file` is missing".into()))?;
    Ok(Upload { file_name, bytes, name, icon })
}

#[utoipa::path(
    post, path = "/api/recipes/{id}/assets", params(("id" = i64, Path, description = "Recipe id")),
    request_body(content_type = "multipart/form-data", content = String, description = "The file in the form field `file` (any type, at most 20 MB); optional text fields `name` and `icon`"),
    responses(
        (status = 201, description = "Attached. The recipe has a new snapshot listing the file's hash", body = Asset),
        (status = 404, description = "No such recipe", body = ErrorBody),
        (status = 422, description = "No file, an empty file, over 20 MB, or the recipe is locked", body = ErrorBody),
    )
)]
pub(crate) async fn upload_asset(sc: Scope, State(s): State<AppState>, UrlPath(id): UrlPath<i64>, form: Multipart) -> Result<(StatusCode, Json<Asset>)> {
    let u = read_upload(form).await?;
    Ok((StatusCode::CREATED, Json(s.db.add_recipe_asset(&sc, id, &s.assets_dir(), &u.file_name, &u.name, &u.icon, &u.bytes).await?)))
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/assets", params(("id" = i64, Path, description = "Recipe id")),
    responses((status = 200, description = "The recipe's attachments, oldest first", body = [Asset]), (status = 404, description = "No such recipe", body = ErrorBody))
)]
pub(crate) async fn list_assets(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>) -> Result<Json<Vec<Asset>>> {
    Ok(Json(db.recipe_assets(&sc, id).await?))
}

#[utoipa::path(
    delete, path = "/api/assets/{id}", params(("id" = String, Path, description = "sha256 of the file"), AssetQuery),
    responses((status = 204, description = "Detached from the recipe (a new snapshot without it)"), (status = 404, description = "No `recipe`, no such recipe, or it does not have that asset", body = ErrorBody), (status = 422, description = "The recipe is locked", body = ErrorBody))
)]
pub(crate) async fn delete_asset(sc: Scope, State(db): State<Db>, UrlPath(hash): UrlPath<String>, Query(q): Query<AssetQuery>) -> Result<StatusCode> {
    db.remove_recipe_asset(&sc, q.recipe.ok_or(Error::NotFound("asset"))?, &hash).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::IntoParams)]
pub(crate) struct AssetQuery {
    /// The recipe the asset is attached to.
    recipe: Option<i64>,
}

#[utoipa::path(
    get, path = "/api/assets/{id}", params(("id" = String, Path, description = "sha256 of the file"), AssetQuery),
    responses(
        (status = 200, description = "The file as uploaded, always `Content-Disposition: attachment` and `X-Content-Type-Options: nosniff`", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 404, description = "No `recipe`, or the recipe (in the caller's group) does not have that asset", body = ErrorBody),
    )
)]
pub(crate) async fn download_asset(sc: Scope, State(s): State<AppState>, UrlPath(hash): UrlPath<String>, Query(q): Query<AssetQuery>) -> Result<Response> {
    let hex_name = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let (true, Some(recipe)) = (hex_name, q.recipe) else { return Err(Error::NotFound("asset")) };
    let asset = s.db.recipe_asset(&sc, recipe, &hash).await?;
    let bytes = tokio::fs::read(s.assets_dir().join(&hash)).await.map_err(|_| Error::NotFound("asset"))?;
    // Never inline, never sniffed, and sandboxed even if something opens it: the bytes are whatever was uploaded.
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", asset.file_name)),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox".to_string()),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CommentInput {
    text: String,
}

#[utoipa::path(
    post, path = "/api/recipes/{id}/comments", params(("id" = i64, Path, description = "Recipe id")), request_body = CommentInput,
    responses((status = 201, description = "Comment added", body = Comment), (status = 404, description = "No such recipe", body = ErrorBody), (status = 422, description = "No text, or the recipe disables comments", body = ErrorBody))
)]
pub(crate) async fn post_comment(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>, Json(c): Json<CommentInput>) -> Result<(StatusCode, Json<Comment>)> {
    Ok((StatusCode::CREATED, Json(db.add_comment(&sc, id, &c.text).await?)))
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/comments", params(("id" = i64, Path, description = "Recipe id")),
    responses((status = 200, description = "Oldest first", body = [Comment]), (status = 404, description = "No such recipe", body = ErrorBody))
)]
pub(crate) async fn list_comments(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>) -> Result<Json<Vec<Comment>>> {
    Ok(Json(db.comments(&sc, id).await?))
}

#[utoipa::path(
    get, path = "/api/foods/{id}", params(("id" = i64, Path, description = "Food id")),
    responses((status = 200, description = "The food with its extras and substitutes", body = FoodDetail), (status = 404, description = "No such food", body = ErrorBody))
)]
pub(crate) async fn get_food(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>) -> Result<Json<FoodDetail>> {
    Ok(Json(db.food(&sc, id).await?))
}

#[utoipa::path(
    put, path = "/api/foods/{id}/extras", params(("id" = i64, Path, description = "Food id")), request_body = BTreeMap<String, String>,
    responses((status = 204, description = "Extras replaced"), (status = 404, description = "No such food", body = ErrorBody), (status = 422, description = "A blank key", body = ErrorBody))
)]
pub(crate) async fn put_food_extras(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>, Json(extras): Json<BTreeMap<String, String>>) -> Result<StatusCode> {
    db.set_food_extras(&sc, id, extras).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/foods/{id}/substitutions", params(("id" = i64, Path, description = "Food id")),
    responses((status = 200, description = "Foods that can stand in, by name", body = [FoodSubstitution]), (status = 404, description = "No such food", body = ErrorBody))
)]
pub(crate) async fn list_food_substitutions(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>) -> Result<Json<Vec<FoodSubstitution>>> {
    Ok(Json(db.food(&sc, id).await?.substitutions))
}

#[utoipa::path(
    post, path = "/api/foods/{id}/substitutions", params(("id" = i64, Path, description = "Food id")), request_body = SubstitutionInput,
    responses((status = 201, description = "Added; the food's substitutes", body = [FoodSubstitution]), (status = 404, description = "No such food", body = ErrorBody), (status = 422, description = "The food itself, or already listed", body = ErrorBody))
)]
pub(crate) async fn add_food_substitution_route(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>, Json(i): Json<SubstitutionInput>) -> Result<(StatusCode, Json<Vec<FoodSubstitution>>)> {
    Ok((StatusCode::CREATED, Json(db.add_food_substitution(&sc, id, i).await?)))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct NoteInput {
    #[serde(default)]
    note: String,
}

#[utoipa::path(
    put, path = "/api/food-substitutions/{id}", params(("id" = i64, Path, description = "Substitution id")), request_body = NoteInput,
    responses((status = 200, description = "Note changed; the food's substitutes", body = [FoodSubstitution]), (status = 404, description = "No such substitution", body = ErrorBody))
)]
pub(crate) async fn put_food_substitution(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>, Json(n): Json<NoteInput>) -> Result<Json<Vec<FoodSubstitution>>> {
    Ok(Json(db.update_food_substitution(&sc, id, &n.note).await?))
}

#[utoipa::path(
    delete, path = "/api/food-substitutions/{id}", params(("id" = i64, Path, description = "Substitution id")),
    responses((status = 204, description = "Removed"), (status = 404, description = "No such substitution", body = ErrorBody))
)]
pub(crate) async fn delete_food_substitution(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>) -> Result<StatusCode> {
    db.remove_food_substitution(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    put, path = "/api/shopping-lists/{id}/extras", params(("id" = i64, Path, description = "List id")), request_body = BTreeMap<String, String>,
    responses((status = 204, description = "Extras replaced"), (status = 404, description = "No such list", body = ErrorBody), (status = 422, description = "A blank key", body = ErrorBody))
)]
pub(crate) async fn put_list_extras(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>, Json(extras): Json<BTreeMap<String, String>>) -> Result<StatusCode> {
    db.set_list_extras(&sc, id, extras).await?;
    Ok(StatusCode::NO_CONTENT)
}
