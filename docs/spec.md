# mealier — spec

Status: agreed 2026-11-05, revised after a Codex review the same day. Revised
2026-11-06: groups and households, full Mealie-export coverage, export and
restore, share links, scheduled webhooks on demand. Steps 1–13 of the build
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
- Images on disk, content-addressed by the sha256 of the stored bytes, through
  one module (`src/images.rs`): sniffed by magic bytes (PNG, JPEG and WebP
  only), refused if wider or taller than 12000 px (read from the header, before
  decoding) or if they do not decode, turned upright, shrunk to at most 2048 px
  on the longest side, re-encoded in the same format (which drops EXIF) and
  never rewritten afterwards. Uploads, URL imports and the Mealie importer all
  use it; `images.mime` records the type and the image route serves from it.
  Shared by recipes, snapshots and batches. An image is deleted only by a
  separate garbage-collection step, never while an export runs, and only when
  nothing references it. Recipe assets (non-image attachments) use the same
  content-addressed store without decoding.
- Errors map to responses in one place (`src/errors.rs`): JSON
  `{"error": "..."}` under `/api/`, an HTML error page (status, message, way
  back, navigation) everywhere else, and a redirect to the sign-in form for
  an unauthenticated page. The path decides, never `Accept`.
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
  plus the original text, and a `uid`: a random id given once and kept by every
  edit that leaves the line alone (the importer uses Mealie's reference id).
  A substitution is `{qty, unit, food, note}` with names copied, not ids.
- **Step**: text, optional title, and links to the ingredient lines it uses (by
  `uid`). Stored beside `steps` as `step_details`, one entry per step, because
  `steps` stays a list of strings.
- **Recipe asset**: a file attached to a recipe (name, icon, cleaned file name),
  stored under its sha256 in `<images dir>/assets/` without decoding.
- **Recipe snapshot**: an immutable copy of a recipe, written on every edit. It
  copies unit and food names (and unit factors), steps, sections, links and
  image and asset hashes, sections, links, substitutions, notes, extras and settings
  as they were, so later renames do not rewrite history. The live recipe points at its
  latest snapshot.
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
- **Meal-plan rule**: group, optional households (none means all of the
  group's), day of week or any, slot or any, and a filter. Used to suggest a
  random matching recipe.
- **Cookbook**: group, name, description, slug, position, public flag, and a
  saved filter in Mealie's filter-string syntax, kept as written, plus its
  parsed form. Membership is computed, not stored.
- **Shopping list** and items (household-scoped); items carry label, extras
  and recipe references.
- **Recipe action**: household, title, kind (`link` or `post`), URL.
- **Share link**: group, recipe, token (stored only as a hash), creator, expiry.
- **Favourite / rating**: user, recipe, favourite flag, rating (half stars, optional).
- **Food on hand / tool owned**: household and a food or tool of its group.

## Features

### Recipes

CRUD in the API and the HTML form for every recipe field: steps, sections,
tags, categories, tools, cookbooks, images, assets, notes, settings, extras.
A name used by a recipe is added to its group's vocabulary if it is new.
Search. Every save writes a snapshot.

- **Favourites and ratings** are the user's own, per recipe (`user_recipes`), for any recipe of their
  group. A rating is Mealie's float kept to half stars, 0.5 to 5, or none. The recipe page has a star
  toggle and a rating select; the recipe list filters to `?favourites=1` (API: `?favourites=true`);
  `GET /api/recipes/{id}/mine`, `PUT .../favourite`, `PUT .../rating`. Archiving a recipe hides it from
  the favourites list but keeps the rows.

- **Form**: a `## ` line in the ingredients box starts a section, in the steps box
  titles the step after it; notes, extras and substitutions are rows (a hidden
  `features` field says the rows are the whole truth; without it they are left
  as they are); a "Link ingredients" page ticks which lines each step uses.
  Cook mode shows a step's linked lines under it, from the batch's snapshot and
  amounts when cooking a batch.
- **Lock**: a locked recipe refuses every edit (including attaching or removing a
  file) with 422 "recipe is locked" unless the request sends `settings` with
  `locked: false`. Leaving `settings`, `notes` or `extras` out of an update keeps
  what the recipe has; `ingredient_details` and `step_details` replace it.
- **Settings**: `locked`, `hide_nutrition` and `disable_comments` are enforced
  (comments are `POST /api/recipes/{id}/comments`); `public`, `show_assets`,
  `landscape_view` and `disable_amount` are Mealie's, kept and ignored.
- **Assets**: any type up to 20 MB, served only by `GET /api/assets/{hash}?recipe=`
  to the recipe's group, always `Content-Disposition: attachment` with
  `X-Content-Type-Options: nosniff`. Export and restore carry them as `assets/<sha256>`.
- **Food substitutions** are group-wide (`/api/foods/{id}/substitutions`,
  `/api/food-substitutions/{id}`); extras on foods, shopping lists and items are
  key/value strings, shown read-only on the list page.

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
actual amounts, never the live recipe. An imported batch has no snapshot, so
it shows the live recipe with a note that the historical version is
unavailable. Ticks are kept in the browser's `localStorage` under
`mealier:cook:recipe:<id>` or `mealier:cook:batch:<id>`; blocked storage just
means they are not remembered, and nothing is sent to the server.

### Meal planner

Plan recipes by date and slot, per household. A plan entry can carry a
`planned` batch (scale and changes). Meal-plan rules suggest a random matching
recipe for an empty day/slot.

### Cookbooks and meal-plan rules

Both hold a filter in Mealie's `query_filter_string` syntax. It is parsed by
`src/filter.rs` and run as a parameterised WHERE over the caller's group's live
recipes; no value is ever spliced into SQL. Supported: `tags`, `recipe_category`
(also `categories`) and `tools`, by `.id` or `.name`, with `IN`, `NOT IN` and
`CONTAINS ALL`; `name` and `description` with `LIKE`, `NOT LIKE`, `=`, `<>`;
`lastMade`, `dateAdded` (`created_at`), `dateUpdated` (`updated_at`) with `=`,
`<>`, `<`, `<=`, `>`, `>=` against a `YYYY-MM-DD` date, and `IS [NOT] NULL`;
`rating` with the same comparisons against a number; `AND`, `OR` (AND binds
tighter) and parentheses; keywords and field spellings are case-insensitive.
`lastMade` reads the caller's household's done batches; `rating` is the caller's
own rating, as in Mealie; `dateAdded` is the
first snapshot's day, `dateUpdated` the current one's. An empty filter selects
every recipe. Anything else is an error naming the token: the API and forms
refuse it (422); the importer keeps the string verbatim, stores no parsed form
and reports it, and the cookbook or rule then shows "filter not supported" and
selects nothing. Ids written by hand are mealier ids; the importer maps
Mealie's uuids (an unknown one becomes 0, which matches nothing).

The cookbook edit page has a builder (tags, categories, tools; any or all)
that writes such a string. `GET /api/cookbooks/{id}/recipes` and
`/cookbooks/{id}` list the matches.

`suggest(scope, date, slot)` takes the rules of the caller's group that apply
to the caller's household on that weekday and slot, keeps the most specific
tier (day and slot, then day, then slot, then neither), pools the recipes those
rules select and picks one with an injectable, seedable generator. No applicable
rule, or a tier that selects nothing, gives none; there is no fall back to a
looser tier. `POST /api/meal-plan/suggest {date, slot}`; the plan page offers
"Suggest" for a day's empty meals and fills the add form.

### Shopping list

Per household. Add items by hand, from a recipe, or from a planned batch
(using its scale, its actual amounts, substitute foods and added lines). Items
are grouped into sections by label: the item's own label, else its food's, else "No label" (last).
Sections follow the list's own order (`shopping_list_labels`: new lists get every group label in name
order; labels without a place sort by name after the placed ones), changed with up/down buttons or
`PUT /api/shopping-lists/{id}/section-order`.

### Foods on hand and tools

Each household marks the foods it has on hand and the tools it owns; the foods and tools are the
group's, the marks are the household's (`household_foods`, `household_tools`). `/pantry` lists
both with a toggle; `GET /api/pantry`, `PUT /api/foods/{id}/on-hand`, `PUT /api/tools/{id}/owned`.
Adding a recipe to a shopping list can skip lines whose food is on hand: a checkbox on the recipe
page, default off (`skip_on_hand` in `POST /api/shopping-lists/{id}/recipes`). Lines with no food
are never skipped.

### Recipe actions

Per household, managed at `/settings/actions` and `/api/recipe-actions`. A **link** action opens a
URL built from a template; the recipe page shows it as `<a rel="noopener noreferrer">`, and only
when the result is an http or https URL. The template uses Mealie's merge fields, so imported
actions work unchanged: `${id}`, `${slug}` (the name, slugified: recipes have no slug column),
`${url}` (the recipe's address, absolute with `MEALIER_BASE_URL`); each value is percent-encoded.
`${servings}`, `${yieldQuantity}` and `${yieldText}` are Mealie's but a mealier recipe has no yield,
so they come out empty; any other `${...}` is left as written. A **post** action sends the recipe
(the API's recipe JSON) to the URL from the server (`POST /api/recipe-actions/{id}/trigger`, or the
button on the recipe page), with the webhook client: internal hosts are allowed, no redirects, 10 s.

Who may write what: any household member creates, edits and deletes **link** actions; creating a
**post** action, or editing one (or turning a link into one), is **admin-only**, because it makes the
server send requests where the admin chose. Any member may delete any action of their household
(deleting sends nothing) and may press a post action an admin set up. A receiver that fails or does
not answer 2xx makes the trigger a 422 naming the action.

### Share links

A share link gives read-only access to one recipe at `/share/<token>` with no
login, until it expires or is revoked. Any member of the recipe's group makes one
(`POST /api/recipes/{id}/share-links`, or the recipe page): 1 to 365 days, default 30. The token is
32 random bytes as base64url, shown once and stored only as its SHA-256; the list shows dates, not
tokens. Revoking deletes the row (`DELETE /api/share-links/{id}`), and any member of the group may.

- **The page** shows the recipe's own content: name, description, image, ingredients (with
  substitutions), steps, notes, and nutrition unless the recipe hides it. No batches, comments,
  household data, tags, extras, source URL, or links to authenticated pages. Assets are listed and
  served only when the recipe's `show_assets` setting is on (now enforced), by
  `/share/<token>/assets/<hash>`; the image by `/share/<token>/images/<name>`. Those are the only
  routes a token opens, each checks the file belongs to the token's recipe, and an archived recipe
  stops being shared.
- **Every share response** (page, 404, redirect, files) carries `Referrer-Policy: no-referrer` and
  `Cache-Control: private, no-store`.
- **Unknown, expired, revoked and malformed** tokens give the same 404 page, byte for byte, with no
  navigation. No rate limit: a token is 256 bits.
- **Mealie's links**: its token is the share row's `id`, shown dashed in the URL
  `/g/<group-slug>/shared/r/<token>`. The importer hashes the dashed form, so `/share/<mealie token>`
  works, and that URL shape redirects (307) to `/share/<token>` without looking the token up.

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
mealier equivalent. They are raised from the store, after the change has
committed, one call site per event, and never for a change that failed. The
importer and restore raise none.

JSON payload: `{"event", "occurred_at", "data"}`. `data` per event:

- `recipe_*`: `recipe_id`, `name`, `group_id`, `url` (the recipe page; `deleted`
  means archived). `recipe_updated` is raised by an edit through the recipe
  form or API or a new image, not by attaching a file, note or link.
- `mealplan_entry_created`: `mealplan_entry_id`, `name` (the entry's title, or
  its recipe's name), `group_id`, `household_id`, `date`, `slot`, `recipe_id`
  (or null), `url` (`/plan`).
- `shopping_list_created`/`_updated`/`_deleted`: `shopping_list_id`, `name`,
  `group_id`, `household_id`, `url` (the list; `/lists` once deleted).
  `_updated` is any change to the list's items or extras. A list is deleted
  with `DELETE /api/shopping-lists/{id}`, which removes its items.
- `cookbook_*`: `cookbook_id`, `name`, `group_id`, `url` (`/cookbooks` once
  deleted).
- `batch_logged`, `import_failed`: as before (their group is known but is not
  in `data`).

`url` is absolute when `MEALIER_BASE_URL` is set. ntfy gets the event name as
its title ("Recipe created"), the name as the body and `url` as the click link.

A webhook may carry a `group_id`: it then receives only events of that group,
and never one that has no group (the "Send test" button ignores the filter).

Notifier addresses hold tokens. The settings page shows a webhook's URL as
scheme and host only (the edit form keeps the saved one when left empty); a
delivery error never repeats the URL; the importer's report names only the
scheme of an address it cannot use. The API returns the URL in full to admins.

### Scheduled webhooks

Mealie's scheduled meal-plan webhooks are kept as webhooks of kind `scheduled`
with a `time_of_day` (`HH:MM`, in the instance timezone: `TZ`, UTC when unset)
and a household (its group follows). The app never runs them by itself.
`mealier run-scheduled` sends each enabled one whose time has passed today and
has not yet been sent today (`webhook_runs` holds one row per webhook and day).
A failed send is recorded with its error and tried again by the next run that
day; a sent one is never sent again that day. Runs are not locked against each
other. An outside timer may call it; nothing does by default. It prints a
count line and exits non-zero if any send failed. Imported ones start
disabled, and so does one made on the settings page unless "enabled" is ticked.

The body is `{"date", "household_id", "group_id", "entries": [{"date", "slot",
"title", "text", "recipe": {"id", "name", "slug", "url"} | null}]}`: the
household's plan for today, breakfast to drink. Like Mealie, an empty plan
sends nothing and records nothing, so a later run that day still sends if
something was planned meanwhile. (Mealie's own body is a list of its plan
entries; the keys here match it loosely, not exactly.) `slug` is derived from
the name, since mealier recipes have none.

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
  the share routes (`/share/<token>`, `/share/<token>/assets/<hash>`, `/share/<token>/images/<name>`,
  `/g/<group>/shared/r/<token>`: whole segments, none empty, nothing else under `/share`).
  Everything else needs a user.

### Backup and export

Export is a zip of a copy of the database made with `VACUUM INTO` (a read
transaction: writers are not held up, and none is held during the zipping)
plus every image and asset it references. Images are immutable and not
garbage-collected during an export, so the pair is consistent. The copy is made
in a staging directory under the data directory and removed afterwards; export
fails, naming the file, if a referenced image is missing or does not match its
hash.

Restore unpacks to `<data dir>/restore-staging` and checks it before touching
anything: every entry name is on an allowlist (`mealier.db`,
`images/<sha256>.<png|jpg|webp>`; nothing with `..`, an absolute path or a
subdirectory), the database opens and passes `quick_check`, it has a migrations
table not newer than this binary (and matching its checksums), and every image
it lists is present with a matching sha256. Only then is the current data
moved to `<data dir>/pre-restore-<timestamp>/` and the staged data moved in; a
failure at any step leaves the current data as it was. The old data is kept,
never deleted by mealier.

Both are admin-only, in the UI (`/settings/backup`) and the API
(`GET /api/export` streams the zip, `POST /api/restore` takes the zip as the
multipart field `file`), and as `mealier export <out.zip>` and
`mealier restore <in.zip>`.

- The server holds a lock on the data directory while it runs. `mealier restore`
  takes the same lock and refuses, saying to stop the server, if it cannot.
  `mealier export` needs no lock and works against a running server.
- The running server cannot keep its old database pool open across a swap, so a
  restore through the UI or API answers `202`, then the process exits (code 0)
  and the container restarts it. Until it has gone, every request except
  `/healthz` gets `503`. The stack must therefore restart the container on exit
  (`restart: unless-stopped`; Sablier starting it on the next request also works).
- The homelab's `docker-volume-backup` stops the container before backing up the
  volume, so that copy is consistent too.

## Migration from Mealie

A one-time importer for **Mealie v3.28 backup zips only**, developed against a
fixture built from fixture data in a real Mealie instance. Every table it maps was
filled in that instance through Mealie's API, so the column names it reads are
Mealie's own (the one exception is the empty legacy link tables, below). It runs in one
transaction into a clean DB: all or nothing, plus a report of skipped records
and referential-integrity checks. It is re-runnable. Cutover: compare counts and
spot-check against Mealie, then point the vhost at mealier. Mealie's volume is
kept until a later prune.

Every table in a v3.28 backup is either mapped or skipped on purpose; a table with rows
that is neither gets an "unknown table" line in the report (name and row count only). Every group
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
  skipped, naming only the scheme. A notifier's group becomes the webhook's group
  filter (its household is not kept). Its `enabled` is kept.
  - Apprise forms: `ntfys://host/topic` is `https://host/topic`, `ntfy://host/topic`
    is `http://host/topic`, `user:pass@` stays in the URL as basic auth,
    `ntfy://topic` is the hosted `https://ntfy.sh/topic`. Several topics, or a
    user without a password (a token), are skipped. A query string (Apprise
    options) is dropped and reported.
  - Options: `group_events_notifier_options` booleans of the same name map to the
    events `recipe_created`/`_updated`/`_deleted`, `mealplan_entry_created`,
    `shopping_list_created`/`_updated`/`_deleted` and
    `cookbook_created`/`_updated`/`_deleted`. The rest (`user_signup`, `data_*`,
    `mealplan_entry_updated`/`_deleted`, `tag_*`, `category_*`, `label_*`) have no
    mealier event and are named on one line per notifier; a notifier left with no
    event is skipped.
- Scheduled webhooks (`webhook_urls`: `group_id`, `household_id`, `name`, `url`,
  `scheduled_time`, the unused `time` as a fallback) are imported disabled. Mealie
  keeps the time in UTC; it is kept as written and one report line says so. A
  URL that is not http(s), or no readable time, skips the row.
- Favourites and ratings: `users_to_recipes` (`user_id`, `recipe_id`, `rating`, `is_favorite`). A
  rating of 0 or none is no rating; others are kept to the nearest half star in 0.5 to 5 and reported
  when that changed them; a row with neither a favourite nor a rating is dropped. A user and recipe of
  different groups refuse the import.
- Recipe actions: `recipe_actions` (`group_id`, `household_id`, `action_type`, `title`, `url`) as
  written, a post action included (an import is the owner's act). No household means the group's first;
  a type other than `link` or `post` is reported and left out.
- Foods on hand and tools owned: `households_to_ingredient_foods` (`household_id`, `food_id`) and
  `households_to_tools` (`household_id`, `tool_id`). The old `on_hand` flags on foods and tools are
  deprecated in Mealie and are not read.
- Meal-plan rules: `group_meal_plan_rules` has its own `household_id`, the household the rule belongs to
  (a v3 rule is per household); the rule is limited to it. The legacy `plan_rules_to_households` can add more.
- Substitutions: a recipe line's (`recipes_ingredients_substitutions`: `ingredient_id`, the integer id of the
  ingredient row, `substitute_food_id`, `note`) may have a note and no food, and it is kept so. A food's
  (`ingredient_foods_substitutions`) must name a food: one with only a note is reported and left out.
- Share links: `recipe_share_tokens` (`id`, `group_id`, `recipe_id`, `expires_at`). The `id` is the
  token; the dashed form its URL shows is hashed, so old links keep working. Expiry is read as UTC;
  one that is not a date is reported and left out. Expired links are imported and answer 404.
- The legacy link tables `cookbooks_to_tags`/`_categories`/`_tools` (`cookbook_id` and `tag_id`,
  `category_id` or `tool_id`) and `plan_rules_to_tags` (`plan_rule_id`), `_categories` and `_households`
  (`group_plan_rule_id`) cannot be filled through Mealie v3.28's API (filters are strings now), so the fixture
  holds none and their tests inject rows; the column names are from Mealie's models. If a backup has rows in
  them they are ANDed onto the filter.
- The fixture's share token is a fake value (`scripts/make-mealie-fixture.py` replaces the real one, which
  opens a fixture recipe on the production Mealie), and its notifier and webhook addresses are fakes
  (`example.invalid`).

**Skipped on purpose, one report line each (when the table has rows; name and row count only, never values):**
`long_live_tokens`, `invite_tokens`, `password_reset_tokens` (secrets and one-time links);
`server_tasks`, `group_reports`, `report_entries`, `group_data_exports`
(Mealie's job history); `ai_providers`, `ai_provider_settings`, `ai_provider_headers`, `ai_provider_params`
(mealier's LLM is configured by environment); `group_to_categories` (an unused link); user passwords.
`alembic_version` is used only for the version check. Three tables are read only to put a line in the
report: `recipe_note_ref_link` (note references on steps), `shopping_list_recipe_reference` (a list's own
recipe references) and `households_to_recipes` (`last_made` with no made-it event).

## Deployment

`stacks/mealier` in the homelab repo: Caddy vhost, `edge` network, Sablier
labels, `docker-volume-backup` labels, Homepage labels, memory limit, stop grace
period, healthcheck via `mealier healthcheck`. Bound to `127.0.0.1` until OIDC
works.

## Build order

Done: 1. Schema, recipes, snapshots. 2. Mealie importer. 3. Batch log. 4. Cook
mode. 5. URL importers and ingredient parser. 6. Meal planner and shopping list.
7. Webhooks. 8. OIDC. 9. LLM fallbacks. Container image and CI. 10. Groups and
households. 11. Review fixes: site-local IPv6, image module (decode, resize,
one copy) with image hashes in snapshots, batch cook mode from the snapshot and
remembered ticks, one quantity-range helper, export and restore, error mapping
in one module with HTML error pages, `web.rs` split by page area, full recipe
form. 12. Labels and shopping-list sections. 13. Cookbook filters and meal-plan
rules. 14. Recipe features: assets, notes, sections, ingredient links, substitutions, extras and settings;
favourites and ratings, recipe actions, foods on hand and tools owned, share links; and their importer mapping.
Filter `rating` reads the caller's own rating (owner approved rewriting
`tests/filter.rs::last_made_and_rating_read_the_callers_household`). 15. Notifier events and scheduled
webhooks (not committed). 16. Fixture and importer coverage for every mapped table (not committed): the
fixture holds real rows of every table Mealie's API can fill, and a test accounts for every table in it.

Remaining: 17. Deploy (`stacks/mealier`).

## STATUS

2026-11-06.

- Phase: step 16 done, not committed (with 15). Fixture rebuilt from a real backup of production Mealie
  with real rows for every table the API can fill; importer fixed where a guess was wrong; every table in
  the fixture is imported or skipped on purpose, asserted against a literal list.
- Blocked on the owner: nothing until step 17 (a GitHub repo, an Authentik application, and
  secrets).
- Next: 17, deploy.
- Production Mealie still holds the `mealier-fixture` items, a second group and household, a second user in
  it, and the backups made along the way; `scripts/cleanup-mealie-fixture.py` (dry run by default)
  removes them once the owner says so.

## STATUS (step 17, 2026-11-06)

- Phase: deployed on `homelab` as `stacks/mealier` (homelab `a4143dd`), image
  `91d3dfb…`, trial backup imported, vhost live, Authentik SSO and local admin
  both tested. `lifecycle: developing`.
- Blocked on the owner: recreating the backup container so `mealier_data` is
  archived.
- Next: after a day of use, `lifecycle: production`; at cutover, clean the
  Mealie fixture data, import a fresh backup, move the `mealie` vhost.
