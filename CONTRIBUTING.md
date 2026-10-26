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

Templates live in `crates/berth-server/assets/templates`, strings in `crates/berth-server/locales/*.yml`: English is the source, every other catalog has the same keys (a test checks it). A new language needs its catalog and an entry in `LOCALES` in `src/admin/i18n.rs`. Debug builds read templates from disk, so a page reload shows the change.

The stylesheet is Tailwind v4, compiled by the standalone CLI (no Node). The output `assets/static/admin.css` is committed; rebuild it after changing templates or `assets/css/admin.css`. CI checks that it is up to date. Download the CLI (v4.3.3, as in CI) for your platform from the [Tailwind releases](https://github.com/tailwindlabs/tailwindcss/releases/tag/v4.3.3) into `.tools/` (git-ignored), then:

```sh
cd crates/berth-server
../../.tools/tailwindcss -i assets/css/admin.css -o assets/static/admin.css --minify
```

## Database migrations

- Migrations live in `crates/berth-server/migrations` and must run on both SQLite and Postgres: no arrays (use join tables), no `AUTOINCREMENT`, no `BLOB`; ids are UUIDv7 `TEXT`, timestamps are epoch milliseconds `BIGINT`.
- Every tenant-owned table has `tenant_id NOT NULL`; composite indexes start with `tenant_id`.
- Schema changes follow expand/contract: a release must work with the schema of the previous release (N-1). Never edit a released migration.

## Releases

Every push to `main` is a release. Raise the patch version by one (`1.0.0` → `1.0.1`) in the same push; bump the minor or major version only when that is decided explicitly.

1. Set the new version in `Cargo.toml` (`[workspace.package] version`) and in the internal dependencies `version = "…"` in `crates/berth-cli/Cargo.toml` and `crates/berth-server/Cargo.toml`.
2. Update `Cargo.lock` and the API snapshot (its `info.version` follows the crate version), then run the checks:

   ```sh
   cargo build
   BERTH_UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot
   cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
   ```

3. Commit the bump as `chore(release): X.Y.Z`, tag it and push both:

   ```sh
   git commit -s -am "chore(release): X.Y.Z"
   git tag vX.Y.Z
   git push origin main vX.Y.Z
   ```

The `v*` tag starts `.github/workflows/release.yml`:

- builds the image for amd64 and arm64 and publishes `ghcr.io/joveptesg/berth` with the tags `X.Y.Z`, `X.Y`, `X` and `latest`;
- creates the GitHub release "Berth X.Y.Z": the subjects of the commits since the previous tag (without the `chore(release)` commit), the install instructions from `.github/release-notes.md` and a link to the full diff.

Commit subjects become the release notes, so write them for the people who install Berth.
