-- Step 13: cookbooks become saved filters, and meal-plan rules arrive (spec, "Cookbook", "Meal-plan rule").
-- `query_filter` is Mealie's filter string exactly as written; `filter_json` is mealier's parse of it
-- (src/filter.rs), NULL when the string is not empty and could not be read. Such a cookbook or rule
-- lists nothing, and says so. Old cookbook rows were bare names, so they keep an empty filter (every
-- recipe) and get a slug of their own.
ALTER TABLE cookbooks ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE cookbooks ADD COLUMN slug TEXT NOT NULL DEFAULT '';
ALTER TABLE cookbooks ADD COLUMN position INTEGER NOT NULL DEFAULT 1;
ALTER TABLE cookbooks ADD COLUMN public INTEGER NOT NULL DEFAULT 0 CHECK (public IN (0, 1));
ALTER TABLE cookbooks ADD COLUMN query_filter TEXT NOT NULL DEFAULT '';
ALTER TABLE cookbooks ADD COLUMN filter_json TEXT;
UPDATE cookbooks SET slug = 'cookbook-' || id;
CREATE UNIQUE INDEX cookbooks_slug ON cookbooks (group_id, slug);

-- Group-scoped. `day` and `slot` are 'unset' for "any". No household rows means every household of the group.
CREATE TABLE meal_plan_rules (
    id INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES groups (id),
    day TEXT NOT NULL DEFAULT 'unset' CHECK (day IN ('monday', 'tuesday', 'wednesday', 'thursday', 'friday', 'saturday', 'sunday', 'unset')),
    slot TEXT NOT NULL DEFAULT 'unset' CHECK (slot IN ('breakfast', 'lunch', 'dinner', 'side', 'snack', 'dessert', 'drink', 'unset')),
    query_filter TEXT NOT NULL DEFAULT '',
    filter_json TEXT
);
CREATE INDEX meal_plan_rules_group ON meal_plan_rules (group_id);

CREATE TABLE meal_plan_rule_households (
    rule_id INTEGER NOT NULL REFERENCES meal_plan_rules (id) ON DELETE CASCADE,
    household_id INTEGER NOT NULL REFERENCES households (id),
    PRIMARY KEY (rule_id, household_id)
);
