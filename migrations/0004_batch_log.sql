-- Index of the snapshot ingredient a batch line started from; NULL for added lines.
ALTER TABLE batch_lines ADD COLUMN position INTEGER;

CREATE TABLE batch_photos (
    batch_id INTEGER NOT NULL REFERENCES batches (id),
    hash TEXT NOT NULL REFERENCES images (hash),
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (batch_id, hash)
);
