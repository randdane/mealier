-- Outgoing notifications (spec, "Notifications"). events is a JSON array of event names.
CREATE TABLE webhooks (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('json', 'ntfy')),
    url TEXT NOT NULL,
    events TEXT NOT NULL,
    ntfy_tags TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
