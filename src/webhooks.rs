//! Webhooks: store (S22) and delivery (S23). Config is admin-only (the `Admin` extractor on its routes).
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

use std::time::Duration;

use crate::{Db, Error, Result, Scope};

pub const EVENTS: [&str; 12] = [
    "import_failed",
    "batch_logged",
    "recipe_created",
    "recipe_updated",
    "recipe_deleted",
    "mealplan_entry_created",
    "shopping_list_created",
    "shopping_list_updated",
    "shopping_list_deleted",
    "cookbook_created",
    "cookbook_updated",
    "cookbook_deleted",
];

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Webhook {
    pub id: i64,
    pub name: String,
    /// `json`, `ntfy` or `scheduled`.
    pub kind: String,
    pub url: String,
    /// A subset of the event names in the spec; empty for `scheduled`.
    pub events: Vec<String>,
    /// ntfy only: comma-separated tags.
    pub ntfy_tags: String,
    pub enabled: bool,
    /// Only this group's events are sent (events without a group are never sent to a filtered webhook).
    pub group_id: Option<i64>,
    /// `scheduled` only: the household whose meal plan is sent.
    pub household_id: Option<i64>,
    /// `scheduled` only: `HH:MM` in the instance timezone.
    pub time_of_day: Option<String>,
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
    #[serde(default)]
    pub group_id: Option<i64>,
    #[serde(default)]
    pub household_id: Option<i64>,
    #[serde(default)]
    pub time_of_day: Option<String>,
}

fn yes() -> bool {
    true
}

impl Webhook {
    /// The URL as shown on screen: scheme and host (and port) only. The rest can carry a token or a password.
    pub fn masked_url(&self) -> String {
        match url::Url::parse(&self.url) {
            Ok(u) => format!("{}://{}{}", u.scheme(), u.host_str().unwrap_or(""), u.port().map(|p| format!(":{p}")).unwrap_or_default()),
            Err(_) => "(not a URL)".into(),
        }
    }
}

/// `HH:MM` as minutes after midnight.
pub(crate) fn parse_hhmm(t: &str) -> Option<u32> {
    let (h, m) = t.split_once(':')?;
    let ok = |x: &str| x.len() == 2 && x.bytes().all(|b| b.is_ascii_digit());
    if !ok(h) || !ok(m) {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// What `validate` settled: the stored events, tags and group filter.
struct Checked {
    events: String,
    ntfy_tags: String,
    group_id: Option<i64>,
}

impl Db {
    async fn validate(&self, i: &WebhookInput) -> Result<Checked> {
        if i.name.trim().is_empty() {
            return Err(Error::Validation("name must not be empty".into()));
        }
        if !["json", "ntfy", "scheduled"].contains(&i.kind.as_str()) {
            return Err(Error::Validation("kind must be json, ntfy or scheduled".into()));
        }
        if !url::Url::parse(&i.url).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some()) {
            return Err(Error::Validation("url must be an http or https URL".into()));
        }
        if i.kind == "scheduled" {
            if i.time_of_day.as_deref().and_then(parse_hhmm).is_none() {
                return Err(Error::Validation("a scheduled webhook needs time_of_day as HH:MM".into()));
            }
            let h = i.household_id.ok_or_else(|| Error::Validation("a scheduled webhook needs a household".into()))?;
            let group: i64 = sqlx::query_scalar("SELECT group_id FROM households WHERE id = ?").bind(h).fetch_optional(&self.pool).await?.ok_or_else(|| Error::Validation("household not found".into()))?;
            if i.group_id.is_some_and(|g| g != group) {
                return Err(Error::Validation("the household is not in that group".into()));
            }
            return Ok(Checked { events: "[]".into(), ntfy_tags: String::new(), group_id: Some(group) });
        }
        if i.time_of_day.is_some() || i.household_id.is_some() {
            return Err(Error::Validation("time_of_day and household_id are for scheduled webhooks only".into()));
        }
        if i.events.is_empty() || i.events.iter().any(|e| !EVENTS.contains(&e.as_str())) {
            return Err(Error::Validation(format!("events must be a non-empty subset of {}", EVENTS.join(", "))));
        }
        if let Some(g) = i.group_id
            && sqlx::query("SELECT 1 FROM groups WHERE id = ?").bind(g).fetch_optional(&self.pool).await?.is_none()
        {
            return Err(Error::Validation("group not found".into()));
        }
        Ok(Checked { events: serde_json::to_string(&i.events)?, ntfy_tags: i.ntfy_tags.clone(), group_id: i.group_id })
    }
}

const COLS: &str = "id, name, kind, url, events, ntfy_tags, enabled, created_at, group_id, household_id, time_of_day";

fn row(r: sqlx::sqlite::SqliteRow) -> Webhook {
    Webhook {
        id: r.get(0),
        name: r.get(1),
        kind: r.get(2),
        url: r.get(3),
        events: serde_json::from_str(r.get::<&str, _>(4)).unwrap_or_default(),
        ntfy_tags: r.get(5),
        enabled: r.get(6),
        group_id: r.get(8),
        household_id: r.get(9),
        time_of_day: r.get(10),
        created_at: r.get(7),
    }
}

impl Db {
    pub async fn create_webhook(&self, i: WebhookInput) -> Result<Webhook> {
        let c = self.validate(&i).await?;
        let id = sqlx::query("INSERT INTO webhooks (name, kind, url, events, ntfy_tags, enabled, group_id, household_id, time_of_day) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(i.name.trim())
            .bind(&i.kind)
            .bind(&i.url)
            .bind(&c.events)
            .bind(&c.ntfy_tags)
            .bind(i.enabled)
            .bind(c.group_id)
            .bind(i.household_id)
            .bind(&i.time_of_day)
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
        let c = self.validate(&i).await?;
        let n = sqlx::query("UPDATE webhooks SET name = ?, kind = ?, url = ?, events = ?, ntfy_tags = ?, enabled = ?, group_id = ?, household_id = ?, time_of_day = ? WHERE id = ?")
            .bind(i.name.trim())
            .bind(&i.kind)
            .bind(&i.url)
            .bind(&c.events)
            .bind(&c.ntfy_tags)
            .bind(i.enabled)
            .bind(c.group_id)
            .bind(i.household_id)
            .bind(&i.time_of_day)
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
    ImportFailed { url: String, reason: String, group_id: i64 },
    BatchLogged { batch_id: i64, recipe_id: i64, recipe_name: String, rating: Option<i64>, made_on: Option<String>, group_id: i64 },
    /// A recipe, meal plan entry, shopping list or cookbook was created, changed or removed; `event` is
    /// the event name (`recipe_created`, ...). Raised by `Db::changed`.
    Changed { event: &'static str, id: i64, name: String, group_id: i64, household_id: Option<i64>, extra: serde_json::Value },
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
        req.send().await.map(|r| r.status().as_u16()).map_err(|e| e.without_url().to_string())
    }

    /// POSTs a JSON body to `url` (a recipe action); the status on any response, the error text otherwise.
    pub(crate) async fn post_json(&self, url: &str, body: String) -> std::result::Result<u16, String> {
        self.client.post(url).header("Content-Type", "application/json").body(body).send().await.map(|r| r.status().as_u16()).map_err(|e| e.without_url().to_string())
    }

    /// `path` made absolute with the base URL, or left as is without one.
    pub(crate) fn link(&self, path: &str) -> String {
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
            Event::Changed { event, .. } => event,
            Event::Test => "test",
        }
    }
    /// The group the event happened in; a webhook filtered to a group gets only its own.
    fn group_id(&self) -> Option<i64> {
        match self {
            Event::ImportFailed { group_id, .. } | Event::BatchLogged { group_id, .. } | Event::Changed { group_id, .. } => Some(*group_id),
            Event::Test => None,
        }
    }
    /// Site path the event is about, if any.
    fn path(&self) -> Option<String> {
        match self {
            Event::BatchLogged { batch_id, .. } => Some(format!("/batches/{batch_id}")),
            // A removed thing has no page of its own: the list it was on instead.
            Event::Changed { event, id, .. } => Some(match *event {
                "mealplan_entry_created" => "/plan".into(),
                "shopping_list_deleted" => "/lists".into(),
                "cookbook_deleted" => "/cookbooks".into(),
                e if e.starts_with("recipe_") => format!("/recipes/{id}"),
                e if e.starts_with("shopping_list_") => format!("/lists/{id}"),
                _ => format!("/cookbooks/{id}"),
            }),
            _ => None,
        }
    }
    fn data(&self, link: impl Fn(&str) -> String) -> serde_json::Value {
        use serde_json::json;
        match self {
            Event::ImportFailed { url, reason, .. } => json!({"url": url, "reason": reason}),
            Event::BatchLogged { batch_id, recipe_id, recipe_name, rating, made_on, .. } => {
                json!({"batch_id": batch_id, "recipe_id": recipe_id, "recipe_name": recipe_name, "rating": rating, "made_on": made_on, "url": link(&format!("/batches/{batch_id}"))})
            }
            Event::Changed { event, id, name, group_id, household_id, extra } => {
                let noun = event.rsplit_once('_').map_or(*event, |(n, _)| n);
                let mut m = serde_json::Map::new();
                m.insert(format!("{noun}_id"), json!(id));
                m.insert("name".into(), json!(name));
                m.insert("group_id".into(), json!(group_id));
                if let Some(h) = household_id {
                    m.insert("household_id".into(), json!(h));
                }
                m.extend(extra.as_object().cloned().unwrap_or_default());
                m.insert("url".into(), json!(self.path().map(|p| link(&p))));
                serde_json::Value::Object(m)
            }
            Event::Test => json!({"message": "Test notification from mealier"}),
        }
    }
    /// ntfy title and body.
    fn text(&self) -> (String, String) {
        match self {
            Event::ImportFailed { url, reason, .. } => ("Import failed".into(), format!("{url}: {reason}")),
            Event::BatchLogged { recipe_name, rating, .. } => (
                format!("Made: {recipe_name}"),
                rating.map_or("Logged".into(), |r| format!("Rated {r}/5")),
            ),
            Event::Changed { event, name, .. } => {
                let title = event.replace('_', " ");
                (format!("{}{}", title[..1].to_uppercase(), &title[1..]), name.clone())
            }
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
        let group = e.group_id();
        tokio::spawn(async move {
            let hooks = match db.list_webhooks().await {
                Ok(h) => h,
                Err(err) => return eprintln!("mealier: webhooks: cannot list: {err}"),
            };
            let at = db.now().await;
            for w in hooks.into_iter().filter(|w| w.enabled && w.events.iter().any(|x| x == e.name()) && (w.group_id.is_none() || w.group_id == group)) {
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

    /// Raises `event` for something that just changed (call after the commit). Meal plans and shopping
    /// lists belong to a household, so theirs go out with the household too.
    pub(crate) fn changed(&self, event: &'static str, sc: &Scope, id: i64, name: impl Into<String>, extra: serde_json::Value) {
        let household_id = (event.starts_with("mealplan_") || event.starts_with("shopping_list_")).then_some(sc.household_id);
        self.notify(Event::Changed { event, id, name: name.into(), group_id: sc.group_id, household_id, extra });
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
