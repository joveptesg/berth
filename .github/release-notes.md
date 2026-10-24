## Install

A Docker image for **amd64 and arm64** (Raspberry Pi, ARM NAS): `ghcr.io/joveptesg/berth:VERSION`.

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the one-time setup link is in the log
```

Update an existing install: `docker compose pull && docker compose up -d`.
The new version backs the database up to `./data/backups` before migrating.

See the [README](https://github.com/joveptesg/berth#running-with-docker) for rollback, backups and HTTPS through a reverse proxy.
