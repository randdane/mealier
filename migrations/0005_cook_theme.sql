-- Per-user cook-mode look; read by the cook page once login exists (step 8).
ALTER TABLE users ADD COLUMN cook_theme TEXT NOT NULL DEFAULT 'normal'
    CHECK (cook_theme IN ('normal', 'high-contrast', 'max-white'));
