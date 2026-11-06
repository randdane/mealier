//! Webhooks: store (S22) and delivery (S23). Config is admin-only (the `Admin` extractor on its routes).
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

use std::time::Duration;

use crate::{Db, Error, Result};

pub const EVENTS: [&str; 2] = ["import_failed", "batch_logged"];

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Webhook {
    pub id: i64,
    pub name: String,
    /// `json` or `ntfy`.
    pub kind: String,
    pub url: String,
    /// Subset of `import_failed`, `batch_logged`.
    pub events: Vec<String>,
    /// ntfy only: comma-separated tags.
    pub ntfy_tags: String,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WebhookInput {
    pub name: String,
    pub kind: String,
    pub url: String,
    pub events: Vec<String>,
    #[serde(default)]
    pub ntfy_tags: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

fn validate(i: &WebhookInput) -> Result<()> {
    if i.name.trim().is_empty() {
        return Err(Error::Validation("name must not be empty".into()));
    }
    if !["json", "ntfy"].contains(&i.kind.as_str()) {
        return Err(Error::Validation("kind must be json or ntfy".into()));
    }
    if !url::Url::parse(&i.url).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some()) {
        return Err(Error::Validation("url must be an http or https URL".into()));
    }
    if i.events.is_empty() || i.events.iter().any(|e| !EVENTS.contains(&e.as_str())) {
        return Err(Error::Validation(format!("events must be a non-empty subset of {}", EVENTS.join(", "))));
    }
    Ok(())
}

const COLS: &str = "id, name, kind, url, events, ntfy_tags, enabled, created_at";

fn row(r: sqlx::sqlite::SqliteRow) -> Webhook {
    Webhook {
        id: r.get(0),
        name: r.get(1),
        kind: r.get(2),
        url: r.get(3),
        events: serde_json::from_str(r.get::<&str, _>(4)).unwrap_or_default(),
        ntfy_tags: r.get(5),
        enabled: r.get(6),
        created_at: r.get(7),
    }
}

impl Db {
    pub async fn create_webhook(&self, i: WebhookInput) -> Result<Webhook> {
        validate(&i)?;
        let id = sqlx::query("INSERT INTO webhooks (name, kind, url, events, ntfy_tags, enabled) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(i.name.trim())
            .bind(&i.kind)
            .bind(&i.url)
            .bind(serde_json::to_string(&i.events)?)
            .bind(&i.ntfy_tags)
            .bind(i.enabled)
            .execute(&self.pool)
            .await?
            .last_insert_rowid();
        self.get_webhook(id).await
    }
    pub async fn list_webhooks(&self) -> Result<Vec<Webhook>> {
        Ok(sqlx::query(&format!("SELECT {COLS} FROM webhooks ORDER BY id")).fetch_all(&self.pool).await?.into_iter().map(row).collect())
    }
    pub async fn get_webhook(&self, id: i64) -> Result<Webhook> {
        sqlx::query(&format!("SELECT {COLS} FROM webhooks WHERE id = ?")).bind(id).fetch_optional(&self.pool).await?.map(row).ok_or(Error::NotFound("webhook"))
    }
    pub async fn update_webhook(&self, id: i64, i: WebhookInput) -> Result<Webhook> {
        validate(&i)?;
        let n = sqlx::query("UPDATE webhooks SET name = ?, kind = ?, url = ?, events = ?, ntfy_tags = ?, enabled = ? WHERE id = ?")
            .bind(i.name.trim())
            .bind(&i.kind)
            .bind(&i.url)
            .bind(serde_json::to_string(&i.events)?)
            .bind(&i.ntfy_tags)
            .bind(i.enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("webhook"));
        }
        self.get_webhook(id).await
    }
    pub async fn delete_webhook(&self, id: i64) -> Result<()> {
        if sqlx::query("DELETE FROM webhooks WHERE id = ?").bind(id).execute(&self.pool).await?.rows_affected() == 0 {
            return Err(Error::NotFound("webhook"));
        }
        Ok(())
    }
}

/// Something worth telling a webhook about.
#[derive(Debug, Clone)]
pub enum Event {
    ImportFailed { url: String, reason: String },
    BatchLogged { batch_id: i64, recipe_id: i64, recipe_name: String, rating: Option<i64>, made_on: Option<String> },
    /// The "Send test" button; ignores the event filter.
    Test,
}

/// Sends events to webhooks. Not the SSRF fetcher: webhooks may target internal
/// hosts (ntfy is one), so this is a plain client that follows no redirects.
#[derive(Clone)]
pub struct Notifier {
    client: reqwest::Client,
    /// `MEALIER_BASE_URL`; absolute links are omitted without it.
    base_url: Option<String>,
}

impl Notifier {
    pub fn new(timeout: Duration, base_url: Option<String>) -> Notifier {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(timeout).user_agent("mealier").build().expect("webhook client");
        Notifier { client, base_url: base_url.map(|b| b.trim_end_matches('/').to_string()).filter(|b| !b.is_empty()) }
    }
    pub fn from_env() -> Notifier {
        Notifier::new(Duration::from_secs(10), std::env::var("MEALIER_BASE_URL").ok())
    }
    /// POSTs `e` to `w`; the status on any response, the error text otherwise.
    async fn deliver(&self, w: &Webhook, e: &Event, occurred_at: &str) -> std::result::Result<u16, String> {
        let req = self.client.post(&w.url);
        let req = if w.kind == "ntfy" {
            let (title, body) = e.text();
            let mut req = req.header("Title", one_line(&title)).body(body);
            if !w.ntfy_tags.is_empty() {
                req = req.header("Tags", one_line(&w.ntfy_tags));
            }
            match e.path().map(|p| self.link(&p)).filter(|l| l.starts_with("http")) {
                Some(click) => req.header("Click", click),
                None => req,
            }
        } else {
            let data = e.data(|p| self.link(p));
            req.header("Content-Type", "application/json").body(serde_json::json!({"event": e.name(), "occurred_at": occurred_at, "data": data}).to_string())
        };
        req.send().await.map(|r| r.status().as_u16()).map_err(|e| e.to_string())
    }

    /// `path` made absolute with the base URL, or left as is without one.
    fn link(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.as_deref().unwrap_or(""))
    }
}

/// Header values cannot hold control characters (a recipe name might).
fn one_line(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Event::ImportFailed { .. } => "import_failed",
            Event::BatchLogged { .. } => "batch_logged",
            Event::Test => "test",
        }
    }
    /// Site path the event is about, if any.
    fn path(&self) -> Option<String> {
        match self {
            Event::BatchLogged { batch_id, .. } => Some(format!("/batches/{batch_id}")),
            _ => None,
        }
    }
    fn data(&self, link: impl Fn(&str) -> String) -> serde_json::Value {
        use serde_json::json;
        match self {
            Event::ImportFailed { url, reason } => json!({"url": url, "reason": reason}),
            Event::BatchLogged { batch_id, recipe_id, recipe_name, rating, made_on } => {
                json!({"batch_id": batch_id, "recipe_id": recipe_id, "recipe_name": recipe_name, "rating": rating, "made_on": made_on, "url": link(&format!("/batches/{batch_id}"))})
            }
            Event::Test => json!({"message": "Test notification from mealier"}),
        }
    }
    /// ntfy title and body.
    fn text(&self) -> (String, String) {
        match self {
            Event::ImportFailed { url, reason } => ("Import failed".into(), format!("{url}: {reason}")),
            Event::BatchLogged { recipe_name, rating, .. } => (
                format!("Made: {recipe_name}"),
                rating.map_or("Logged".into(), |r| format!("Rated {r}/5")),
            ),
            Event::Test => ("Mealier test".into(), "Test notification".into()),
        }
    }
}

impl Db {
    pub fn with_notifier(mut self, n: Notifier) -> Db {
        self.notifier = n;
        self
    }
    /// Fire and forget: returns at once, delivery happens on tasks. Call after the
    /// triggering transaction has committed. Failures are logged, never retried.
    // ponytail: no retries; add a retry/outbox if deliveries get lost.
    pub fn notify(&self, e: Event) {
        let db = self.clone();
        tokio::spawn(async move {
            let hooks = match db.list_webhooks().await {
                Ok(h) => h,
                Err(err) => return eprintln!("mealier: webhooks: cannot list: {err}"),
            };
            let at = db.now().await;
            for w in hooks.into_iter().filter(|w| w.enabled && w.events.iter().any(|x| x == e.name())) {
                let (n, e, at) = (db.notifier.clone(), e.clone(), at.clone());
                tokio::spawn(async move {
                    match n.deliver(&w, &e, &at).await {
                        Ok(s) if (200..300).contains(&s) => {}
                        Ok(s) => eprintln!("mealier: webhook {} ({}) answered {s}", w.name, e.name()),
                        Err(err) => eprintln!("mealier: webhook {} ({}) failed: {err}", w.name, e.name()),
                    }
                });
            }
        });
    }

    async fn now(&self) -> String {
        sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')").fetch_one(&self.pool).await.unwrap_or_default()
    }

    /// Delivers a test event to one webhook (enabled or not) and returns the HTTP status.
    pub async fn send_test(&self, id: i64) -> Result<u16> {
        let w = self.get_webhook(id).await?;
        let at = self.now().await;
        self.notifier.deliver(&w, &Event::Test, &at).await.map_err(|e| Error::Validation(format!("delivery failed: {e}")))
    }
}
