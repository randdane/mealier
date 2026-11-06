-- Step 10: tenancy (spec, "Tenancy"). A group is a tenant; a household belongs to one group.
-- Group-scoped tables gain group_id, household-scoped ones household_id, users gain household_id,
-- all NOT NULL, and names that were unique for the whole instance become unique per group.
--
-- SQLite cannot add NOT NULL or change a UNIQUE in place, so each table is rebuilt: create the new
-- one, copy every row with its id, drop the old, rename. That is only safe with foreign keys OFF:
-- with them on, dropping `users` would cascade into sessions and api_tokens. `Db::connect` runs
-- migrations on a connection with foreign keys off (a migration's own transaction cannot change the
-- pragma), checks `PRAGMA foreign_key_check` afterwards, and switches them on. The guard below
-- makes any other runner fail rather than lose rows.
CREATE TEMP TABLE migration_needs_foreign_keys_off (ok INTEGER NOT NULL CHECK (ok = 0));
INSERT INTO migration_needs_foreign_keys_off SELECT foreign_keys FROM pragma_foreign_keys;
DROP TABLE migration_needs_foreign_keys_off;

CREATE TABLE groups (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    slug TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE households (
    id INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES groups (id),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    slug TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (group_id, slug)
);

-- Mealie's group_preferences / household_preferences: the fields mealier reads are columns,
-- every other field of the Mealie row is kept as JSON in `extra`.
CREATE TABLE group_preferences (
    group_id INTEGER PRIMARY KEY REFERENCES groups (id) ON DELETE CASCADE,
    private_group INTEGER NOT NULL DEFAULT 1,
    extra TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE household_preferences (
    household_id INTEGER PRIMARY KEY REFERENCES households (id) ON DELETE CASCADE,
    first_day_of_week INTEGER NOT NULL DEFAULT 0,
    private_household INTEGER NOT NULL DEFAULT 1,
    extra TEXT NOT NULL DEFAULT '{}'
);

-- key `default_household_id`: the household new users join.
CREATE TABLE instance_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Existing data goes into "Home" / "Home", which is also the default household. A database with no
-- rows gets no tenant until one is needed (`Db::default_household`).
INSERT INTO groups (name, slug)
SELECT 'Home', 'home'
WHERE EXISTS (SELECT 1 FROM users) OR EXISTS (SELECT 1 FROM recipes) OR EXISTS (SELECT 1 FROM units)
   OR EXISTS (SELECT 1 FROM foods) OR EXISTS (SELECT 1 FROM tags) OR EXISTS (SELECT 1 FROM categories)
   OR EXISTS (SELECT 1 FROM cookbooks) OR EXISTS (SELECT 1 FROM tools) OR EXISTS (SELECT 1 FROM batches)
   OR EXISTS (SELECT 1 FROM meal_plan_entries) OR EXISTS (SELECT 1 FROM shopping_lists);
INSERT INTO households (group_id, name, slug) SELECT id, 'Home', 'home' FROM groups;
INSERT INTO group_preferences (group_id) SELECT id FROM groups;
INSERT INTO household_preferences (household_id) SELECT id FROM households;
INSERT INTO instance_settings (key, value) SELECT 'default_household_id', CAST(id AS TEXT) FROM households;

-- ---- group-scoped ----
CREATE TABLE units_new (
    id INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES groups (id),
    name TEXT NOT NULL,
    aliases TEXT NOT NULL DEFAULT '[]',
    dimension TEXT NOT NULL CHECK (dimension IN ('mass', 'volume', 'count', 'other')),
    factor REAL,
    CHECK (
        (dimension IN ('mass', 'volume') AND factor IS NOT NULL AND factor > 0)
        OR (dimension IN ('count', 'other') AND factor IS NULL)
    ),
    UNIQUE (group_id, name)
);
INSERT INTO units_new (id, group_id, name, aliases, dimension, factor)
    SELECT id, (SELECT id FROM groups), name, aliases, dimension, factor FROM units;
DROP TABLE units;
ALTER TABLE units_new RENAME TO units;

CREATE TABLE foods_new (
    id INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES groups (id),
    name TEXT NOT NULL,
    aliases TEXT NOT NULL DEFAULT '[]',
    UNIQUE (group_id, name)
);
INSERT INTO foods_new (id, group_id, name, aliases) SELECT id, (SELECT id FROM groups), name, aliases FROM foods;
DROP TABLE foods;
ALTER TABLE foods_new RENAME TO foods;

CREATE TABLE tags_new (id INTEGER PRIMARY KEY, group_id INTEGER NOT NULL REFERENCES groups (id), name TEXT NOT NULL, UNIQUE (group_id, name));
INSERT INTO tags_new (id, group_id, name) SELECT id, (SELECT id FROM groups), name FROM tags;
DROP TABLE tags;
ALTER TABLE tags_new RENAME TO tags;

CREATE TABLE categories_new (id INTEGER PRIMARY KEY, group_id INTEGER NOT NULL REFERENCES groups (id), name TEXT NOT NULL, UNIQUE (group_id, name));
INSERT INTO categories_new (id, group_id, name) SELECT id, (SELECT id FROM groups), name FROM categories;
DROP TABLE categories;
ALTER TABLE categories_new RENAME TO categories;

CREATE TABLE tools_new (id INTEGER PRIMARY KEY, group_id INTEGER NOT NULL REFERENCES groups (id), name TEXT NOT NULL, UNIQUE (group_id, name));
INSERT INTO tools_new (id, group_id, name) SELECT id, (SELECT id FROM groups), name FROM tools;
DROP TABLE tools;
ALTER TABLE tools_new RENAME TO tools;

CREATE TABLE cookbooks_new (id INTEGER PRIMARY KEY, group_id INTEGER NOT NULL REFERENCES groups (id), name TEXT NOT NULL, UNIQUE (group_id, name));
INSERT INTO cookbooks_new (id, group_id, name) SELECT id, (SELECT id FROM groups), name FROM cookbooks;
DROP TABLE cookbooks;
ALTER TABLE cookbooks_new RENAME TO cookbooks;

-- recipe_snapshots has no group of its own: it is reached through its recipe, and its immutability
-- triggers are untouched because that table is not rebuilt.
CREATE TABLE recipes_new (
    id INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES groups (id),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    description TEXT NOT NULL DEFAULT '',
    source_url TEXT,
    nutrition TEXT, -- JSON, nullable
    archived INTEGER NOT NULL DEFAULT 0,
    current_snapshot_id INTEGER REFERENCES recipe_snapshots (id)
);
INSERT INTO recipes_new (id, group_id, name, description, source_url, nutrition, archived, current_snapshot_id)
    SELECT id, (SELECT id FROM groups), name, description, source_url, nutrition, archived, current_snapshot_id FROM recipes;
DROP TABLE recipes;
ALTER TABLE recipes_new RENAME TO recipes;
CREATE INDEX recipes_group ON recipes (group_id, name);

-- ---- household-scoped ----
CREATE TABLE batches_new (
    id INTEGER PRIMARY KEY,
    household_id INTEGER NOT NULL REFERENCES households (id),
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
INSERT INTO batches_new (id, household_id, status, recipe_id, snapshot_id, source, made_on, scale, rating, notes)
    SELECT id, (SELECT id FROM households), status, recipe_id, snapshot_id, source, made_on, scale, rating, notes FROM batches;
DROP TABLE batches;
ALTER TABLE batches_new RENAME TO batches;
CREATE INDEX batches_recipe ON batches (recipe_id, made_on);
CREATE INDEX batches_household ON batches (household_id);

CREATE TABLE meal_plan_entries_new (
    id INTEGER PRIMARY KEY,
    household_id INTEGER NOT NULL REFERENCES households (id),
    date TEXT NOT NULL, -- YYYY-MM-DD
    slot TEXT NOT NULL,
    recipe_id INTEGER REFERENCES recipes (id),
    title TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL DEFAULT '',
    batch_id INTEGER REFERENCES batches (id)
);
INSERT INTO meal_plan_entries_new (id, household_id, date, slot, recipe_id, title, text, batch_id)
    SELECT id, (SELECT id FROM households), date, slot, recipe_id, title, text, batch_id FROM meal_plan_entries;
DROP TABLE meal_plan_entries;
ALTER TABLE meal_plan_entries_new RENAME TO meal_plan_entries;
CREATE INDEX meal_plan_date ON meal_plan_entries (household_id, date);

CREATE TABLE shopping_lists_new (
    id INTEGER PRIMARY KEY,
    household_id INTEGER NOT NULL REFERENCES households (id),
    name TEXT NOT NULL
);
INSERT INTO shopping_lists_new (id, household_id, name) SELECT id, (SELECT id FROM households), name FROM shopping_lists;
DROP TABLE shopping_lists;
ALTER TABLE shopping_lists_new RENAME TO shopping_lists;
CREATE INDEX shopping_lists_household ON shopping_lists (household_id);

-- ---- users ----
CREATE TABLE users_new (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL UNIQUE,
    full_name TEXT NOT NULL DEFAULT '',
    role TEXT NOT NULL CHECK (role IN ('admin', 'user')),
    oidc_issuer TEXT,
    oidc_subject TEXT,
    disabled INTEGER NOT NULL DEFAULT 0,
    cook_theme TEXT NOT NULL DEFAULT 'normal' CHECK (cook_theme IN ('normal', 'high-contrast', 'max-white')),
    local_admin INTEGER NOT NULL DEFAULT 0 CHECK (local_admin IN (0, 1)),
    household_id INTEGER NOT NULL REFERENCES households (id),
    UNIQUE (oidc_issuer, oidc_subject)
);
INSERT INTO users_new (id, username, email, full_name, role, oidc_issuer, oidc_subject, disabled, cook_theme, local_admin, household_id)
    SELECT id, username, email, full_name, role, oidc_issuer, oidc_subject, disabled, cook_theme, local_admin, (SELECT id FROM households) FROM users;
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;
CREATE INDEX users_household ON users (household_id);
-- The break-glass admin: at most one, never bound to OIDC (as in 0008).
CREATE UNIQUE INDEX users_one_local_admin ON users (local_admin) WHERE local_admin = 1;
CREATE TRIGGER users_local_admin_no_oidc_insert BEFORE INSERT ON users
WHEN NEW.local_admin = 1 AND (NEW.oidc_issuer IS NOT NULL OR NEW.oidc_subject IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'the local admin cannot have an OIDC binding'); END;
CREATE TRIGGER users_local_admin_no_oidc_update BEFORE UPDATE ON users
WHEN NEW.local_admin = 1 AND (NEW.oidc_issuer IS NOT NULL OR NEW.oidc_subject IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'the local admin cannot have an OIDC binding'); END;
