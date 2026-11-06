# mealier — spec

Status: agreed 2026-11-05, revised after a Codex review the same day. Revised
2026-11-06: groups and households, full Mealie-export coverage, export and
restore, share links, scheduled webhooks on demand. Steps 1–10 of the build
order are implemented; the remaining work is under "Build order".

## Goal

A Rust rewrite of Mealie, run in the homelab as `stacks/mealier`. It keeps
Mealie's features and adds a **batch log**: a record of each time a recipe was
made, with the amounts actually used. Anyone with a Mealie v3.28 export must be
able to migrate without silent loss: every feature that can appear in an export
is supported, or deliberately skipped and reported (see "Migration from
Mealie"). It is a hobby project; RAM is a secondary goal (Mealie is already
stopped when idle by Sablier). Target: under 30 MB RSS idle, measured against
Mealie and recorded in the README.

## Non-goals (for now)

- A batch-vs-recipe diff view, promoting a batch to a new recipe version, and
  per-recipe charts across batches.
- Native screen brightness control (browsers cannot do it).
- Mealie API compatibility.
- Running timers inside the app: Sablier stops it when idle. Scheduled work
  runs only when something outside calls `mealier run-scheduled` (see
  "Scheduled webhooks").

## Architecture

- One binary: axum, askama templates, htmx for partial updates. Server-rendered
  HTML; no SPA.
- SQLite in WAL mode through `sqlx`, with migrations in the repo.
- Images on disk, content-addressed by hash, decoded and resized on upload
  (longest side at most 2048 px; PNG, JPEG and WebP only), never rewritten.
  Shared by recipes, snapshots and batches. An image is deleted only by a
  separate garbage-collection step, never while an export runs, and only when
  nothing references it. Recipe assets (non-image attachments) use the same
  content-addressed store without decoding.
- Container: multi-stage build to a static musl binary in a distroless image,
  published to GHCR by CI.
- Data directory (DB + images + assets) is a single volume.
- Own REST API under `/api`, described by an OpenAPI spec generated from the
  code.
- Configuration is environment variables only (set in the stack's `.env`,
  editable only by the host owner). Nothing security-relevant is configurable
  from the UI or API.

### Lifecycle

Sablier stops the container when idle, so stop/start is routine.

- Readiness is reported only after migrations have run.
- On SIGTERM: stop accepting requests, let in-flight transactions finish within
  the stop grace period, then exit. Long jobs (import, export, restore) run in
  transactions or write to a staging path, so an interrupted job leaves no
  partial state.
- Unclean stops rely on SQLite WAL recovery at next open.

## Domain

### Tenancy

- **Group**: a fully separate tenant; groups share nothing. An instance holds
  one or more groups.
- **Household**: belongs to one group. Every user belongs to exactly one
  household.
- **Group-scoped** (shared by every household in the group): recipes and their
  snapshots, units, foods, labels, tags, categories, tools, cookbooks,
  meal-plan rules, share links.
- **Household-scoped**: batches, meal plans, shopping lists, recipe actions,
  foods on hand, tools owned, per-household preferences.
- **User-scoped**: favourites, ratings, cook theme, API tokens.
- **Instance-scoped** (admin): webhooks and notifiers, users, LLM settings,
  export and restore. Webhooks may be filtered to one group.
- A user sees only their own group's data, and only their own household's
  household-scoped data. Admins manage users and instance settings but see
  data under the same rules.
- The scope (user, group, household, admin) is derived from the user's row on
  every request, and every store call takes it. Something in another group or
  household answers 404, the same as something that does not exist. Records
  that point at others (a batch's recipe, a plan entry's recipe and batch, a
  shopping item's unit, food and recipe) must be in the caller's scope too.
  Images are served only to the group of a recipe that uses them or the
  household of a batch that does.
- Names (units, foods, tags, categories, tools, cookbooks) are unique per
  group. Group and household preferences keep the fields mealier reads as
  columns (`private_group`, `first_day_of_week`, `private_household`) and the
  rest of Mealie's row as JSON.
- The **default household** is an instance setting an admin chooses on
  `/settings/households` (or `PUT /api/default-household`). OIDC users and the
  local admin join it when their account is created; nothing is made until
  needed, then "Home" in "Home". Admins create groups and households and move
  users, across groups too; a moved user's old household keeps its data.

### Records

- **User**: roles admin and user; household; cook theme.
- **Recipe**: name, description, steps, ingredient sections, tags, categories,
  tools, source URL, images, assets, notes, nutrition fields, extras (key/value),
  settings (locked, hide nutrition, disable comments). Deleting a recipe
  archives it; its snapshots and batches are kept.
- **Unit**: name, aliases, dimension (`mass`, `volume`, `count`, `other`) and,
  for mass and volume, a factor to the base unit (g, ml).
- **Food**: name, aliases, label, substitutions (other foods), extras.
- **Label**: name, colour. Used to group shopping-list items into sections.
- **Ingredient line**: quantity as `qty_min`/`qty_max` (equal when not a range;
  both empty for "to taste"), unit, food, note, section title, substitutions,
  plus the original text.
- **Step**: text, optional title, and links to the ingredient lines it uses.
- **Recipe snapshot**: an immutable copy of a recipe, written on every edit. It
  copies unit and food names (and unit factors), steps, sections, links and
  image hashes as they were, so later renames do not rewrite history. The live
  recipe points at its latest snapshot.
- **Batch**: one making of a recipe by a household. Fields: status (`planned`
  or `done`), snapshot (optional; empty for imported history), source
  (`native` or `imported`), date, scale factor, rating (1–5, optional), notes,
  up to 5 photos.
- **Batch line**: one per snapshot ingredient, holding the actual amount
  (quantity + unit; empty means unknown, not zero), a substitution text and an
  optional substitute food. Extra lines may be added; these are flagged `added`,
  belong to the batch only and still reference the shared unit/food tables.
  Adding them never changes the recipe or creates a snapshot.
- **Meal plan entry**: household, date, meal slot, recipe or title + text,
  optionally a `planned` batch.
- **Meal-plan rule**: group, optional households, day of week or any, slot or
  any, tag/category filters. Used to suggest a random matching recipe.
- **Cookbook**: group, name, description, and a saved filter (tags,
  categories, tools, and a free-form query compatible with Mealie's filter
  strings where it maps). Membership is computed, not stored.
- **Shopping list** and items (household-scoped); items carry label, extras
  and recipe references.
- **Recipe action**: household, title, kind (`link` or `post`), URL template.
- **Share link**: group, recipe, token (stored only as a hash), expiry.

## Features

### Recipes

CRUD in the API and the HTML form for every recipe field: steps, sections,
tags, categories, tools, cookbooks, images, assets, notes, settings, extras.
Search. Every save writes a snapshot. Locked recipes refuse edits until
unlocked. Favourites and ratings per user.

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
`normal`, `high-contrast` or `max-white`. Steps can be ticked off. Cooking a
batch shows that batch's snapshot (name, steps, sections) with the batch's
actual amounts, never the live recipe.

### Meal planner

Plan recipes by date and slot, per household. A plan entry can carry a
`planned` batch (scale and changes). Meal-plan rules suggest a random matching
recipe for an empty day/slot.

### Shopping list

Per household. Add items by hand, from a recipe, or from a planned batch
(using its scale, its actual amounts, substitute foods and added lines). Items
are grouped by their food's label.

### Foods on hand and tools

Each household can mark foods it has on hand and tools it owns. Adding a recipe
to a shopping list can skip on-hand foods.

### Recipe actions

Per household: a link action opens a URL built from a template with the
recipe's fields; a post action sends the recipe as JSON to a URL (same
delivery rules as webhooks).

### Share links

A share link gives read-only access to one recipe at `/share/<token>` with no
login, until it expires or is revoked. The page shows the recipe only: no
batches, comments or household data. Tokens are random, stored only as a hash;
imported Mealie share tokens keep working.

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
not be loopback, private, link-local, site-local or otherwise internal, checked
on every redirect; bounded redirects, response size and time.

**`exec` rules:**

- Commands are defined only in configuration (`MEALIER_EXEC_IMPORTERS` in the
  stack's `.env`), never through the UI or API.
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

Generic JSON webhooks (URL + event filter, optionally one group) and an ntfy
preset (title, tags, click URL). Only admins configure them. Webhooks may
target internal hosts (ntfy is one); the fetch restrictions above apply to URL
import only. Events:

- `batch_logged`, `import_failed`
- `recipe_created`, `recipe_updated`, `recipe_deleted`
- `mealplan_entry_created`
- `shopping_list_created`, `shopping_list_updated`, `shopping_list_deleted`
- `cookbook_created`, `cookbook_updated`, `cookbook_deleted`

These cover every event a Mealie v3.28 notifier can subscribe to that has a
mealier equivalent.

### Scheduled webhooks

Mealie's scheduled meal-plan webhooks are kept as webhooks of kind `scheduled`
with a time of day. The app never runs them by itself. `mealier run-scheduled`
sends each enabled one whose time has passed today and has not yet been sent
today (a per-day key prevents duplicates). An outside timer may call it;
nothing does by default. Imported ones start disabled.

### Auth

- OIDC with Authentik, the only configured issuer. Email is trusted from it
  even though Authentik sends `email_verified: false`.
- First login links to an existing user by email, rejecting ambiguous matches;
  after that the user is bound to issuer + subject, not email.
- Group `mealier-users` may sign in; membership of `mealier-admins` is checked
  on every login and grants or removes admin.
- A new user joins the **default household** an admin has chosen; admins can
  move users between households.
- Sessions last 7 days. Disabling a user revokes their sessions and API tokens
  at once.
- A local admin password remains as break-glass when Authentik is down. No
  auto-redirect to OIDC.
- Signup off.
- API access uses per-user tokens.
- Public routes: `/healthz`, `/login*`, `/auth/callback`, `/static/*` and
  `/share/<token>`. Everything else needs a user.

### Backup and export

Export is a zip of a DB copy made with the SQLite backup API plus every image
and asset it references; images are immutable and not garbage-collected during
an export, so the pair is consistent. Restore unpacks to a staging directory,
validates it, then swaps it in, keeping the previous data until the swap
succeeds. Both are admin-only, in the UI and as `mealier export` /
`mealier restore`. The homelab's `docker-volume-backup` stops the container
before backing up the volume, so that copy is consistent too.

## Migration from Mealie

A one-time importer for **Mealie v3.28 backup zips only**, developed against a
fixture built from fixture data in a real Mealie instance. It runs in one
transaction into a clean DB: all or nothing, plus a report of skipped records
and referential-integrity checks. It is re-runnable. Cutover: compare counts and
spot-check against Mealie, then point the vhost at mealier. Mealie's volume is
kept until a later prune.

Every table in a v3.28 backup is either mapped or skipped on purpose. Every group
and household in the backup is imported; each record goes to its own group or
household (a household-scoped record without a usable user goes to its group's
first household), and the first household becomes the default. A backup whose
records cross groups is refused whole. "Clean" means empty apart from the
local admin and the empty "Home" made for them: the admin moves into the
default household and Home is removed.

**Mapped:** groups, households and their preferences; users (linked as in
Auth; no passwords); recipes with ingredients, sections, instructions, ingredient
links, substitutions, nutrition, settings, extras, notes, assets, images,
comments, tags, categories, tools; foods, units, aliases, labels, food extras
and substitutions; cookbooks with their filters; meal plans and meal-plan rules;
shopping lists with items, labels, extras and recipe references; recipe
actions; foods on hand and tools owned; favourites and ratings; share links;
notifiers; scheduled webhooks.

- Timeline "made" events become `done` batches with `source = imported`, no
  snapshot and unknown amounts.
- Mealie has no quantity ranges: `qty_min = qty_max`, original text kept.
- Notifiers: Apprise `ntfy://`/`ntfys://` URLs become ntfy webhooks,
  `json://`/`jsons://` become JSON webhooks; other schemes are reported as
  skipped. Subscribed events map to the mealier events above.
- Scheduled webhooks are imported disabled.

**Skipped on purpose, one report line each:** `long_live_tokens`,
`invite_tokens`, `password_reset_tokens` (secrets and one-time links);
`server_tasks`, `group_reports`, `report_entries`, `group_data_exports`
(Mealie's job history); `alembic_version` (used only for the version check);
`ai_providers*` (mealier's LLM is configured by environment); user passwords.

## Deployment

`stacks/mealier` in the homelab repo: Caddy vhost, `edge` network, Sablier
labels, `docker-volume-backup` labels, Homepage labels, memory limit, stop grace
period, healthcheck via `mealier healthcheck`. Bound to `127.0.0.1` until OIDC
works.

## Build order

Done: 1. Schema, recipes, snapshots. 2. Mealie importer. 3. Batch log. 4. Cook
mode. 5. URL importers and ingredient parser. 6. Meal planner and shopping list.
7. Webhooks. 8. OIDC. 9. LLM fallbacks. Container image and CI. 10. Groups and
households.

Remaining, in order:

11. Review fixes: site-local IPv6, image hashes in snapshots, batch cook mode
    from the snapshot, export and restore, full recipe form, image decode and
    resize, one image module, one quantity-range helper, error mapping in one
    module with HTML error pages, `web.rs` split by page area.
12. Labels and shopping-list sections.
13. Cookbook filters and meal-plan rules.
14. Recipe features: assets, notes, sections, ingredient links, substitutions,
    extras, favourites and ratings, settings, recipe actions, foods on hand and
    tools owned, share links.
15. Notifier events and scheduled webhooks.
16. Fixture and importer coverage for every mapped table.
17. Deploy (`stacks/mealier`).

## STATUS

2026-11-06, paused for the session token limit.

- Phase: step 10 (groups and households) done and committed; 341 tests pass,
  clippy clean.
- In flight: nothing. No agents or servers running.
- Blocked on the owner: nothing until deploy (step 17 needs a GitHub repo,
  an Authentik application, and secrets).
- Next: step 11, review fixes (list under "Build order"). Then 12–16 without
  stopping unless a decision is needed.
- Production Mealie still holds the `mealier-fixture` items and three extra
  backups; `scripts/cleanup-mealie-fixture.py` removes the items after step 16
  (step 16 adds more fixture data first).
