-- SQLite cannot add a CHECK to an existing table, so rebuild units.
-- Nothing references units (ingredient lines hold copies in snapshot JSON).
CREATE TABLE units_new (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    aliases TEXT NOT NULL DEFAULT '[]',
    dimension TEXT NOT NULL CHECK (dimension IN ('mass', 'volume', 'count', 'other')),
    factor REAL,
    CHECK (
        (dimension IN ('mass', 'volume') AND factor IS NOT NULL AND factor > 0)
        OR (dimension IN ('count', 'other') AND factor IS NULL)
    )
);
INSERT INTO units_new (id, name, aliases, dimension, factor)
    SELECT id, name, aliases, dimension, factor FROM units;
DROP TABLE units;
ALTER TABLE units_new RENAME TO units;

-- Tags, categories and cookbooks are read from the current snapshot only;
-- nothing filters on these join tables.
DROP TABLE recipe_tags;
DROP TABLE recipe_categories;
DROP TABLE recipe_cookbooks;
