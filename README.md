# mealier

A small Rust rewrite of Mealie: recipes, meal planner, shopping list and a batch log of what you actually made.
One static binary, SQLite, server-rendered HTML. See `docs/spec.md`.

## Run locally

```bash
MEALIER_INSECURE_COOKIES=1 MEALIER_ADMIN_PASSWORD_FILE=./admin.hash cargo run   # http://127.0.0.1:9000
```

Data goes to `$XDG_DATA_HOME/_r_/mealier` (default `~/.local/share/_r_/mealier`) unless `MEALIER_DATA_DIR` is set.

Container:

```bash
docker build -t mealier:dev .
docker run -d -p 127.0.0.1:9000:9000 -v mealier-data:/data mealier:dev   # /data must be writable by uid 65532
```

CI publishes `ghcr.io/<owner>/mealier:latest` and `:<sha>` from `main`.

## Commands

| Command | What |
|---|---|
| `mealier` | run the server |
| `mealier hash-password` | read a password on stdin, print an argon2id hash for `MEALIER_ADMIN_PASSWORD_FILE` |
| `mealier healthcheck` | GET `127.0.0.1:$PORT/healthz`, exit 0 on 200 (the image's `HEALTHCHECK`) |
| `mealier import-mealie <zip>` | import a Mealie backup into the data dir and print the report; stop the server first |

```bash
printf '%s' 'my password' | mealier hash-password > admin.hash && chmod 600 admin.hash
```

## Environment

Secrets are read from files, never from the environment. Files must not be group/other readable (LLM key).

| Variable | Default | Meaning |
|---|---|---|
| `MEALIER_DATA_DIR` | `$XDG_DATA_HOME/_r_/mealier` (image: `/data`) | DB and images |
| `BIND_ADDR` | `127.0.0.1` (image: `0.0.0.0`) | listen address |
| `PORT` | `9000` | listen port (also used by `healthcheck`) |
| `MEALIER_BASE_URL` | unset | public http(s) URL; form POSTs must come from its origin; required with OIDC |
| `MEALIER_INSECURE_COOKIES` | unset | `1` drops the Secure cookie flag, for plain-http development only |
| `MEALIER_ADMIN_PASSWORD_FILE` | unset | file with the argon2id hash; enables break-glass local login |
| `OIDC_CONFIGURATION_URL` | unset | OIDC discovery URL; setting it enables OIDC |
| `OIDC_CLIENT_ID` | | required with OIDC |
| `OIDC_CLIENT_SECRET_FILE` | | required with OIDC |
| `OIDC_USER_GROUP` | `mealier-users` | group allowed to sign in |
| `OIDC_ADMIN_GROUP` | `mealier-admins` | group that gets the admin role |
| `MEALIER_IMPORTERS` | `jsonld,microdata` (`,llm` added when an LLM is configured) | URL-import chain, in order |
| `MEALIER_EXEC_IMPORTERS` | unset | JSON argv arrays for external importers, placed where `exec` appears in `MEALIER_IMPORTERS` |
| `LLM_PROVIDER` | unset | `anthropic`, `openai` or `openrouter`; enables the LLM features |
| `LLM_MODEL` | | required with `LLM_PROVIDER` |
| `LLM_API_KEY_FILE` | | required with `LLM_PROVIDER`, mode 600 |
| `LLM_BASE_URL` | provider default | override the API endpoint |
| `HOME`, `XDG_DATA_HOME` | | only used to find the default data dir |

## Memory

Spec target: under 30 MB RSS idle. Measured 2026-11-06, release build in the distroless image (21.7 MB),
`docker stats` after 30 s idle, three readings: **2.9 MiB, 2.5 MiB, 2.5 MiB**. Mealie idles at 220-450 MB.
`docker stats` reports cgroup memory without page cache; it is a fresh empty database, so expect growth with data and traffic.
