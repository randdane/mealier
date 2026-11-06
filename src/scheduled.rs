//! Scheduled webhooks (spec, "Scheduled webhooks"): `mealier run-scheduled` sends each enabled one whose time has passed.
use serde_json::{Value, json};

use crate::{Db, Result, Scope, Webhook};

/// "Now" as the schedule sees it: the date and the minutes since midnight, in the instance timezone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clock {
    /// YYYY-MM-DD
    pub date: String,
    pub minutes: u32,
}

impl Clock {
    /// A unix time read in UTC, or in the process's timezone (`TZ`) when `local`.
    pub fn from_unix(ts: i64, local: bool) -> Clock {
        let t = ts as libc::time_t;
        // SAFETY: `tm` is plain data; `localtime_r`/`gmtime_r` fill it from `t` and keep no pointer to it.
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if local {
                libc::localtime_r(&t, &mut tm);
            } else {
                libc::gmtime_r(&t, &mut tm);
            }
            tm
        };
        Clock { date: format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday), minutes: (tm.tm_hour * 60 + tm.tm_min) as u32 }
    }

    /// The instance timezone is `TZ`; unset or empty means UTC (not the machine's `/etc/localtime`).
    pub fn now() -> Clock {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        Clock::from_unix(secs, std::env::var_os("TZ").is_some_and(|v| !v.is_empty()))
    }
}

/// What one `run_scheduled` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    pub sent: usize,
    pub failed: usize,
    /// Enabled, but its time has not come yet today.
    pub not_due: usize,
    /// Already sent today.
    pub already_sent: usize,
    /// Nothing planned for today, so nothing was sent (Mealie sends nothing either).
    pub empty: usize,
}

impl Db {
    /// Sends the plan of the day `now.date` to every enabled scheduled webhook whose time has passed and that
    /// has not been sent today. A failed send is recorded and tried again by the next run that day.
    // ponytail: runs are not locked against each other; two overlapping runs could both send. A timer does not overlap itself.
    pub async fn run_scheduled(&self, now: &Clock) -> Result<RunReport> {
        let mut report = RunReport::default();
        for w in self.list_webhooks().await?.into_iter().filter(|w| w.kind == "scheduled" && w.enabled) {
            if now.minutes < w.time_of_day.as_deref().and_then(crate::webhooks::parse_hhmm).unwrap_or(u32::MAX) {
                report.not_due += 1;
                continue;
            }
            let done: Option<String> = sqlx::query_scalar("SELECT status FROM webhook_runs WHERE webhook_id = ? AND run_date = ?").bind(w.id).bind(&now.date).fetch_optional(&self.pool).await?;
            if done.as_deref() == Some("sent") {
                report.already_sent += 1;
                continue;
            }
            let Some(body) = self.day_plan(&w, &now.date).await? else {
                report.empty += 1;
                continue;
            };
            let outcome = match self.notifier.post_json(&w.url, body.to_string()).await {
                Ok(s) if (200..300).contains(&s) => Ok(()),
                Ok(s) => Err(format!("the receiver answered {s}")),
                Err(e) => Err(e),
            };
            sqlx::query(
                "INSERT INTO webhook_runs (webhook_id, run_date, status, error) VALUES (?, ?, ?, ?)
                 ON CONFLICT (webhook_id, run_date) DO UPDATE SET status = excluded.status, error = excluded.error, attempts = attempts + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            )
            .bind(w.id)
            .bind(&now.date)
            .bind(if outcome.is_ok() { "sent" } else { "failed" })
            .bind(outcome.as_ref().err().cloned().unwrap_or_default())
            .execute(&self.pool)
            .await?;
            match outcome {
                Ok(()) => report.sent += 1,
                Err(e) => {
                    report.failed += 1;
                    eprintln!("mealier: scheduled webhook {} failed: {e}", w.name);
                }
            }
        }
        Ok(report)
    }

    /// The household's plan for `date` as the body of a scheduled webhook; `None` when nothing is planned.
    async fn day_plan(&self, w: &Webhook, date: &str) -> Result<Option<Value>> {
        let (Some(group_id), Some(household_id)) = (w.group_id, w.household_id) else { return Ok(None) };
        let sc = Scope { user_id: 0, group_id, household_id, admin: false };
        let mut entries = self.meal_plan(&sc, date, date).await?;
        if entries.is_empty() {
            return Ok(None);
        }
        entries.sort_by_key(|e| (crate::SLOTS.iter().position(|s| *s == e.slot), e.id));
        let mut out = Vec::new();
        for e in entries {
            let recipe = match e.recipe_id {
                Some(id) => self.get_recipe(&sc, id).await?.map(|r| json!({"id": id, "name": r.content.name, "slug": crate::tenancy::slugify(&r.content.name, "recipe"), "url": self.notifier.link(&format!("/recipes/{id}"))})),
                None => None,
            };
            out.push(json!({"date": e.date, "slot": e.slot, "title": e.title, "text": e.text, "recipe": recipe}));
        }
        Ok(Some(json!({"date": date, "household_id": household_id, "group_id": group_id, "entries": out})))
    }
}
