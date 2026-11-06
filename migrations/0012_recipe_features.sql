-- Step 14, part A: recipe assets, extras on foods and shopping lists, food substitutions.
-- Everything else a recipe gained (section titles, step titles and links, line substitutions, notes,
-- extras, settings) lives in the snapshot JSON, so history is frozen with it.

-- Content-addressed, like images, but never decoded: the file is <images dir>/assets/<hash>.
CREATE TABLE assets (hash TEXT PRIMARY KEY, size INTEGER NOT NULL);
CREATE TABLE recipe_assets (
    id INTEGER PRIMARY KEY,
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    hash TEXT NOT NULL REFERENCES assets (hash),
    name TEXT NOT NULL,
    icon TEXT NOT NULL DEFAULT '',
    file_name TEXT NOT NULL,
    UNIQUE (recipe_id, hash)
);

-- key/value extras as a JSON object of strings.
ALTER TABLE foods ADD COLUMN extras TEXT NOT NULL DEFAULT '{}';
ALTER TABLE shopping_lists ADD COLUMN extras TEXT NOT NULL DEFAULT '{}';
ALTER TABLE shopping_list_items ADD COLUMN extras TEXT NOT NULL DEFAULT '{}';

-- Group-scoped (both foods are checked against the caller's group): food -> other foods that can stand in.
CREATE TABLE food_substitutions (
    id INTEGER PRIMARY KEY,
    food_id INTEGER NOT NULL REFERENCES foods (id) ON DELETE CASCADE,
    substitute_food_id INTEGER NOT NULL REFERENCES foods (id) ON DELETE CASCADE,
    note TEXT NOT NULL DEFAULT '',
    UNIQUE (food_id, substitute_food_id),
    CHECK (food_id <> substitute_food_id)
);
