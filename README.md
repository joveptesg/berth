# Berth

Self-hosted audiobook server with pluggable storage.

- **Database is the source of truth** — books are entities, not scanned folders.
- **Pluggable storage** — local disk, any S3-compatible service (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **Delivery separated from storage** — the server authorizes, bytes can come straight from object storage or a CDN via short-lived signed URLs.
- **Web admin** for setup, uploads and users; phones connect by QR code.

> **Status:** early development. Nothing to install yet.

## License

[Apache License 2.0](LICENSE)
