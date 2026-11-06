-- Step 12: labels (spec, "Label") group the shopping list into sections.
-- Group-scoped, names unique per group. A food carries a default label; an item's own label, when set,
-- overrides its food's (as in Mealie). `shopping_list_labels` is the per-list section order.
-- Deleting a label clears it from foods and items and drops it from every list's order.
CREATE TABLE labels (
    id INTEGER PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES groups (id),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    color TEXT NOT NULL DEFAULT '#959595' CHECK (color GLOB '#[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]'),
    UNIQUE (group_id, name)
);

ALTER TABLE foods ADD COLUMN label_id INTEGER REFERENCES labels (id) ON DELETE SET NULL;
ALTER TABLE shopping_list_items ADD COLUMN label_id INTEGER REFERENCES labels (id) ON DELETE SET NULL;

CREATE TABLE shopping_list_labels (
    list_id INTEGER NOT NULL REFERENCES shopping_lists (id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (list_id, label_id)
);
