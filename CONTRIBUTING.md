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

## Database migrations

- Migrations live in `crates/berth-server/migrations` and must run on both SQLite and Postgres: no arrays (use join tables), no `AUTOINCREMENT`, no `BLOB`; ids are UUIDv7 `TEXT`, timestamps are epoch milliseconds `BIGINT`.
- Every tenant-owned table has `tenant_id NOT NULL`; composite indexes start with `tenant_id`.
- Schema changes follow expand/contract: a release must work with the schema of the previous release (N-1). Never edit a released migration.
