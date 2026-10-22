# Berth

Self-hosted audiobook server with pluggable storage.

- **Database is the source of truth** — books are entities, not scanned folders.
- **Pluggable storage** — local disk, any S3-compatible service (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **Delivery separated from storage** — the server authorizes, bytes can come straight from object storage or a CDN via short-lived signed URLs.
- **Web admin** for setup, uploads and users; phones connect by QR code.

> **Status:** early development. Nothing to install yet.

## Running with Docker

```sh
docker compose up -d --build      # http://localhost:8080, data in ./data
docker compose logs berth         # the one-time setup link is in the log
```

`./data` holds the database, its backups, the local storage and `secret.key`. Keep a copy of
`secret.key` apart from the backups: storage keys in the database cannot be read without it.

### Updating and rolling back

```sh
git pull && docker compose up -d --build
```

Before it changes the database schema, the new version saves a copy to `./data/backups`
(the last 3 are kept; they are also listed under Settings → Backup in the admin). To roll back,
check out the previous version and restore the copy made before the update:

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d --build
```

### Backups and restore

- Settings → Backup in the admin downloads a copy of the database and restores one: from an
  automatic backup or an uploaded file. The current database is saved to the automatic backups
  first, so a restore can be undone the same way.
- From the console (server stopped): `berth admin restore <file>`.
## License

[Apache License 2.0](LICENSE)
