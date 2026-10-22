# Contributing

## Developer Certificate of Origin

Contributions are accepted under the [Developer Certificate of Origin](https://developercertificate.org/) (DCO); no CLA is required. Sign off every commit:

```sh
git commit -s
```

CI rejects pull requests with commits that lack a `Signed-off-by:` line.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/) in English, e.g. `feat: add storage profiles`.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`openapi.json` at the repository root is the API contract; `cargo test` fails when the API changes. Review the change and update the snapshot:

```sh
BERTH_UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot
```

## Web admin

Templates live in `crates/berth-server/assets/templates`, strings in `crates/berth-server/locales/{en,ru}.yml` (same keys in both files). Debug builds read templates from disk, so a page reload shows the change.

The stylesheet is Tailwind v4, compiled by the standalone CLI (no Node). The output `assets/static/admin.css` is committed; rebuild it after changing templates or `assets/css/admin.css`. CI checks that it is up to date. Download the CLI (v4.3.3, as in CI) for your platform from the [Tailwind releases](https://github.com/tailwindlabs/tailwindcss/releases/tag/v4.3.3) into `.tools/` (git-ignored), then:

```sh
cd crates/berth-server
../../.tools/tailwindcss -i assets/css/admin.css -o assets/static/admin.css --minify
```

## Database migrations

- Migrations live in `crates/berth-server/migrations` and must run on both SQLite and Postgres: no arrays (use join tables), no `AUTOINCREMENT`, no `BLOB`; ids are UUIDv7 `TEXT`, timestamps are epoch milliseconds `BIGINT`.
- Every tenant-owned table has `tenant_id NOT NULL`; composite indexes start with `tenant_id`.
- Schema changes follow expand/contract: a release must work with the schema of the previous release (N-1). Never edit a released migration.
