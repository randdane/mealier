-- Step 14, part B: favourites and ratings (user), recipe actions, foods on hand and tools owned
-- (household), share links (group).

-- One row per user and recipe. The rating is Mealie's float, kept to half stars.
CREATE TABLE user_recipes (
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    is_favorite INTEGER NOT NULL DEFAULT 0,
    rating REAL CHECK (rating IS NULL OR (rating >= 0.5 AND rating <= 5 AND rating * 2 = round(rating * 2))),
    PRIMARY KEY (user_id, recipe_id)
);

CREATE TABLE recipe_actions (
    id INTEGER PRIMARY KEY,
    household_id INTEGER NOT NULL REFERENCES households (id),
    title TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('link', 'post')),
    url TEXT NOT NULL
);

-- Both sides are checked against the caller's group when a row is written; the household is the caller's own.
CREATE TABLE household_foods (
    household_id INTEGER NOT NULL REFERENCES households (id),
    food_id INTEGER NOT NULL REFERENCES foods (id) ON DELETE CASCADE,
    PRIMARY KEY (household_id, food_id)
);
CREATE TABLE household_tools (
    household_id INTEGER NOT NULL REFERENCES households (id),
    tool_id INTEGER NOT NULL REFERENCES tools (id) ON DELETE CASCADE,
    PRIMARY KEY (household_id, tool_id)
);

-- The token itself is never stored: only its SHA-256 (hex). Times are UTC, 'YYYY-MM-DDTHH:MM:SSZ'.
CREATE TABLE share_links (
    id INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES groups (id),
    recipe_id INTEGER NOT NULL REFERENCES recipes (id),
    token_hash TEXT NOT NULL UNIQUE,
    created_by INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX share_links_recipe ON share_links (recipe_id);
