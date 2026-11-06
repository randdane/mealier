-- Step 15 (spec, "Notifications" and "Scheduled webhooks"): webhooks gain an optional group filter and the
-- kind `scheduled` (a household's meal plan sent at a time of day), and each day's send is recorded.
-- The CHECK on kind cannot be altered, so the table is rebuilt; nothing references it.
CREATE TABLE webhooks_new (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('json', 'ntfy', 'scheduled')),
    url TEXT NOT NULL,
    events TEXT NOT NULL,
    ntfy_tags TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- only this group's events; NULL means every group's
    group_id INTEGER REFERENCES groups (id),
    -- scheduled only
    household_id INTEGER REFERENCES households (id),
    time_of_day TEXT CHECK (time_of_day GLOB '[0-2][0-9]:[0-5][0-9]'),
    CHECK ((kind = 'scheduled') = (household_id IS NOT NULL AND time_of_day IS NOT NULL))
);
INSERT INTO webhooks_new (id, name, kind, url, events, ntfy_tags, enabled, created_at) SELECT id, name, kind, url, events, ntfy_tags, enabled, created_at FROM webhooks;
DROP TABLE webhooks;
ALTER TABLE webhooks_new RENAME TO webhooks;

-- One row per (webhook, day) once a send has been tried. `sent` is final; `failed` is tried again on the next run that day.
CREATE TABLE webhook_runs (
    webhook_id INTEGER NOT NULL REFERENCES webhooks (id) ON DELETE CASCADE,
    run_date TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('sent', 'failed')),
    error TEXT NOT NULL DEFAULT '',
    attempts INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (webhook_id, run_date)
);
