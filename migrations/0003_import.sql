CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL UNIQUE,
    full_name TEXT NOT NULL DEFAULT '',
    role TEXT NOT NULL CHECK (role IN ('admin', 'user')),
    oidc_issuer TEXT,
    oidc_subject TEXT,
    disabled INTEGER NOT NULL DEFAULT 0,
    UNIQUE (oidc_issuer, oidc_subject)
);

-- Content-addressed: the file is <data_dir>/images/<hash>.<ext>.
CREATE TABLE images (hash TEXT PRIMARY KEY, mime TEXT NOT NULL);
CREATE TABLE recipe_images (
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    hash TEXT NOT NULL REFERENCES images (hash),
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (recipe_id, hash)
);

-- Like tags: the recipe's tool names live in its snapshot; this is the vocabulary.
CREATE TABLE tools (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

CREATE TABLE recipe_comments (
    id INTEGER PRIMARY KEY,
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    user_id INTEGER REFERENCES users (id),
    text TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE batches (
    id INTEGER PRIMARY KEY,
    status TEXT NOT NULL CHECK (status IN ('planned', 'done')),
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    snapshot_id INTEGER REFERENCES recipe_snapshots (id),
    source TEXT NOT NULL CHECK (source IN ('native', 'imported')),
    made_on TEXT, -- YYYY-MM-DD
    scale REAL NOT NULL DEFAULT 1 CHECK (scale > 0),
    rating INTEGER CHECK (rating BETWEEN 1 AND 5),
    notes TEXT NOT NULL DEFAULT '',
    CHECK (snapshot_id IS NOT NULL OR source = 'imported')
);
CREATE INDEX batches_recipe ON batches (recipe_id, made_on);

CREATE TABLE batch_lines (
    id INTEGER PRIMARY KEY,
    batch_id INTEGER NOT NULL REFERENCES batches (id),
    qty_min REAL, -- NULL means unknown, not zero
    qty_max REAL,
    unit_id INTEGER REFERENCES units (id),
    food_id INTEGER REFERENCES foods (id),
    substitution TEXT NOT NULL DEFAULT '',
    substitute_food_id INTEGER REFERENCES foods (id),
    added INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE meal_plan_entries (
    id INTEGER PRIMARY KEY,
    date TEXT NOT NULL, -- YYYY-MM-DD
    slot TEXT NOT NULL,
    recipe_id INTEGER REFERENCES recipes (id),
    title TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL DEFAULT '',
    batch_id INTEGER REFERENCES batches (id)
);
CREATE INDEX meal_plan_date ON meal_plan_entries (date);

CREATE TABLE shopping_lists (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE shopping_list_items (
    id INTEGER PRIMARY KEY,
    list_id INTEGER NOT NULL REFERENCES shopping_lists (id),
    qty REAL, -- NULL means no amount
    unit_id INTEGER REFERENCES units (id),
    food_id INTEGER REFERENCES foods (id),
    note TEXT NOT NULL DEFAULT '',
    checked INTEGER NOT NULL DEFAULT 0,
    recipe_id INTEGER REFERENCES recipes (id)
);

-- Used by the Mealie importer only: Mealie's UUID -> local id, kept so a
-- cutover can be spot-checked against Mealie.
CREATE TABLE import_ids (
    kind TEXT NOT NULL,
    mealie_uuid TEXT NOT NULL,
    local_id INTEGER NOT NULL,
    PRIMARY KEY (kind, mealie_uuid)
);
