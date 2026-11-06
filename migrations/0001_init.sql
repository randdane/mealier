CREATE TABLE units (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    aliases TEXT NOT NULL DEFAULT '[]',
    dimension TEXT NOT NULL CHECK (dimension IN ('mass', 'volume', 'count', 'other')),
    factor REAL
);

CREATE TABLE foods (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    aliases TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE categories (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE cookbooks (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

-- Ingredient lines and steps live only in the snapshot JSON; the live recipe
-- reads them from its current snapshot.
CREATE TABLE recipes (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    description TEXT NOT NULL DEFAULT '',
    source_url TEXT,
    nutrition TEXT, -- JSON, nullable
    archived INTEGER NOT NULL DEFAULT 0,
    current_snapshot_id INTEGER REFERENCES recipe_snapshots (id)
);

CREATE TABLE recipe_snapshots (
    id INTEGER PRIMARY KEY,
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    content TEXT NOT NULL, -- full JSON copy of the recipe
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX recipe_snapshots_recipe ON recipe_snapshots (recipe_id, id);

CREATE TABLE recipe_tags (
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    tag_id INTEGER NOT NULL REFERENCES tags (id),
    PRIMARY KEY (recipe_id, tag_id)
);
CREATE TABLE recipe_categories (
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    category_id INTEGER NOT NULL REFERENCES categories (id),
    PRIMARY KEY (recipe_id, category_id)
);
CREATE TABLE recipe_cookbooks (
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    cookbook_id INTEGER NOT NULL REFERENCES cookbooks (id),
    PRIMARY KEY (recipe_id, cookbook_id)
);

CREATE TRIGGER recipe_snapshots_no_update BEFORE UPDATE ON recipe_snapshots
BEGIN SELECT RAISE(ABORT, 'recipe_snapshots are immutable'); END;
CREATE TRIGGER recipe_snapshots_no_delete BEFORE DELETE ON recipe_snapshots
BEGIN SELECT RAISE(ABORT, 'recipe_snapshots are immutable'); END;
