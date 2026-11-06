# mealier — spec

Status: agreed 2026-11-05, revised the same day after a Codex review. Before
implementation.

## Goal

A Rust rewrite of Mealie, run in the homelab as `stacks/mealier`. It keeps
Mealie's features and adds a **batch log**: a record of each time a recipe was
made, with the amounts actually used. It is a hobby project; RAM is a secondary
goal (Mealie is already stopped when idle by Sablier). Target: under 30 MB RSS
idle, measured against Mealie and recorded in the README.

## Non-goals (for now)

- A batch-vs-recipe diff view, promoting a batch to a new recipe version, and
  per-recipe charts across batches.
- Native screen brightness control (browsers cannot do it).
- Mealie API compatibility.
- More than one household.
- Scheduled notifications: Sablier stops the app when idle, so it cannot run
  its own timers.

## Architecture

- One binary: axum, askama templates, htmx for partial updates. Server-rendered
  HTML; no SPA.
- SQLite in WAL mode through `sqlx`, with migrations in the repo.
- Images on disk, content-addressed by hash, resized on upload, never rewritten.
  Shared by recipes, snapshots and batches. An image is deleted only by a
  separate garbage-collection step, never while an export runs, and only when
  nothing references it.
- Container: multi-stage build to a static musl binary in a distroless image,
  published to GHCR by CI.
- Data directory (DB + images) is a single volume.
- Own REST API under `/api`, described by an OpenAPI spec generated from the
  code.

### Lifecycle

Sablier stops the container when idle, so stop/start is routine.

- Readiness is reported only after migrations have run.
- On SIGTERM: stop accepting requests, let in-flight transactions finish within
  the stop grace period, then exit. Long jobs (import, export) run in
  transactions or write to a staging path, so an interrupted job leaves no
  partial state.
- Unclean stops rely on SQLite WAL recovery at next open.

## Domain

- **Household**: exactly one. Several **users** belong to it. Roles: admin,
  user.
- **Recipe**: name, description, steps, tags, categories, cookbooks, source URL,
  images, nutrition fields, ingredients. Deleting a recipe archives it; its
  snapshots and batches are kept.
- **Unit**: name, aliases, dimension (`mass`, `volume`, `count`, `other`) and,
  for mass and volume, a factor to the base unit (g, ml).
- **Food**: name, aliases.
- **Ingredient line**: quantity as `qty_min`/`qty_max` (equal when not a range;
  both empty for "to taste"), unit, food, note, plus the original text.
- **Recipe snapshot**: an immutable copy of a recipe, written on every edit. It
  copies unit and food names (and unit factors) as they were, so later renames
  do not rewrite history. The live recipe points at its latest snapshot.
- **Batch**: one making of a recipe. Fields: status (`planned` or `done`),
  snapshot (optional; empty for imported history), source (`native` or
  `imported`), date, scale factor, rating (1–5, optional), notes, up to 5
  photos.
- **Batch line**: one per snapshot ingredient, holding the actual amount
  (quantity + unit; empty means unknown, not zero), a substitution text and an
  optional substitute food. Extra lines may be added; these are flagged `added`,
  belong to the batch only and still reference the shared unit/food tables.
  Adding them never changes the recipe or creates a snapshot.
- **Meal plan entry**: date, meal slot, recipe, optionally a `planned` batch.
- **Shopping list** and items.

## Features

### Recipes

CRUD, tags, categories, cookbooks, search, images. Every save writes a
snapshot.

### Units

Conversion only within `mass` or within `volume`, through the base-unit factor.
`count` and `other` never convert. Never between mass and volume. Scaling
multiplies `qty_min` and `qty_max`. Shopping-list aggregation sums lines only
when food matches and units convert.

### Nutrition

Plain editable fields. A button asks the configured LLM to estimate them; the
result fills the form for the user to save, it is not written automatically.

### Batch log

- "Start a batch" on a recipe: choose a scale; opens a table of the snapshot's
  ingredients with the scaled amount pre-filled in the *actual* column. Every
  field is editable.
- Columns: ingredient, recipe amount (scaled), actual amount, substitution.
- "Add ingredient" appends an `added` line.
- Rating, notes and photos on the batch.
- A batch is `planned` until marked done; marking it `done` raises
  `batch_logged`.
- The recipe page lists its batches, newest first.

### Cook mode

Screen Wake Lock keeps the screen on. A per-user setting, **cook theme**:
`normal`, `high-contrast` or `max-white`. Steps can be ticked off.

### Meal planner

Plan recipes by date and slot. A plan entry can carry a `planned` batch (scale
and changes).

### Shopping list

Add items by hand, from a recipe, or from a planned batch (using its scale, its
actual amounts, substitute foods and added lines).

### URL import (plugin chain)

Importers are tried in an order set in config; the first that returns a recipe
wins. Each returns schema.org `Recipe` JSON-LD, which one mapper turns into a
recipe.

Built-in importers (Rust traits, compiled in):

1. `jsonld`: embedded `application/ld+json`.
2. `microdata`: schema.org microdata / HTML fallback.
3. `llm`: sends page text to the configured LLM, expects JSON-LD back.
4. `exec`: runs an external command; URL and HTML on stdin, JSON-LD on stdout.

**Fetching** (all importers): `http`/`https` only; the resolved address must
not be loopback, private, link-local or otherwise internal, checked on every
redirect; bounded redirects, response size and time.

**`exec` rules:**

- Commands are defined only in the config file, never through the UI or API.
- Run directly from argv, no shell, with a cleared environment.
- Bounded stdout/stderr; on timeout the whole process tree is killed.
- The LLM key is read once at startup and never passed to children; logs never
  contain it.
- Plugins must be standalone executables mounted into the container. The
  distroless image ships no interpreters; a plugin needing one means building an
  image on top of mealier's.

Failed imports are logged and raise `import_failed`.

### Ingredient parsing

1. Rule-based parser: quantity (including fractions and ranges), unit (from the
   units table with aliases), food, note.
2. Lines the rules cannot parse go to the configured LLM, if one is set.
3. A review screen shows every parsed line for confirmation or correction.
   Confirmed units and foods are added to the shared tables.

### LLM

One provider active at a time, used for import, parsing fallback and nutrition.

| Variable | Meaning |
|---|---|
| `LLM_PROVIDER` | `anthropic`, `openai` or `openrouter`; unset disables LLM features |
| `LLM_MODEL` | model ID for that provider |
| `LLM_API_KEY_FILE` | path to a file (mode 600) holding the key |

The settings page shows the active provider and model and has a "test" button.
The key is never shown or logged.

### Notifications

Generic JSON webhooks (URL + event filter) and an ntfy preset (title, tags,
click URL). Only admins configure them. Webhooks may target internal hosts
(ntfy is one); the fetch restrictions above apply to URL import only. Events:

- `import_failed`
- `batch_logged`

### Auth

- OIDC with Authentik, the only configured issuer. Email is trusted from it
  even though Authentik sends `email_verified: false`.
- First login links to an existing user by email, rejecting ambiguous matches;
  after that the user is bound to issuer + subject, not email.
- Group `mealier-users` may sign in; membership of `mealier-admins` is checked
  on every login and grants or removes admin.
- Sessions last 7 days. Disabling a user revokes their sessions and API tokens
  at once.
- A local admin password remains as break-glass when Authentik is down. No
  auto-redirect to OIDC.
- Signup off.
- API access uses per-user tokens.

### Backup and export

Export is a zip of a DB copy made with the SQLite backup API plus every image
it references; images are immutable and not garbage-collected during an export,
so the pair is consistent. Restore unpacks to a staging directory, validates
it, then swaps it in, keeping the previous data until the swap succeeds. The
homelab's `docker-volume-backup` stops the container before backing up the
volume, so that copy is consistent too.

## Migration from Mealie

A one-time importer for **Mealie v3.28 backup zips only**, developed against a
real backup as its fixture. It maps recipes, images, units, foods, ingredients
(parsed where Mealie parsed them, raw text kept otherwise), tags, categories,
cookbooks, meal plans, shopping lists, users (linked as in Auth) and timeline
"made" events. Made events become `done` batches with `source = imported`, no
snapshot and unknown amounts. If the backup has several households, the user
picks one.

The import runs in one transaction into a clean DB: all or nothing, plus a
report of skipped records and referential-integrity checks. It is re-runnable.
Cutover: compare counts and spot-check against Mealie, then point the vhost at
mealier. Mealie's volume is kept until a later prune.

## Deployment

`stacks/mealier` in the homelab repo: Caddy vhost, `edge` network, Sablier
labels, `docker-volume-backup` labels, Homepage labels, memory limit, stop grace
period. Bound to `127.0.0.1` until OIDC works.

## Build order

1. Schema, recipes, snapshots.
2. Mealie importer.
3. Batch log.
4. Cook mode.
5. URL importers and ingredient parser.
6. Meal planner and shopping list.
7. Webhooks.
8. OIDC (before any exposure beyond `127.0.0.1`).
9. LLM fallbacks.
