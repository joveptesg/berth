<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — selbst gehosteter Hörbuch-Server" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <b>Deutsch</b> · <a href="README.es.md">Español</a> · <a href="README.fr.md">Français</a> · <a href="README.it.md">Italiano</a> · <a href="README.pt-BR.md">Português (Brasil)</a> · <a href="README.zh-Hans.md">简体中文</a> · <a href="README.ja.md">日本語</a> · <a href="README.ko.md">한국어</a>
</p>

Berth ist ein selbst gehosteter Hörbuch-Server für dich und deine Familie. Lade Bücher über den Web-Admin hoch, bewahre die Dateien in einem beliebigen Speicher auf und höre sie in der Berth-App für Android und iOS.

## Warum Berth

- **Die Datenbank ist die maßgebliche Quelle** — Bücher sind Einträge in der Datenbank, keine gescannten Ordner.
- **Austauschbarer Speicher** — lokale Festplatte, jeder S3-kompatible Dienst (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **Auslieferung getrennt vom Speicher** — der Server prüft die Berechtigung, die Daten selbst können über kurzlebige signierte Links direkt aus dem Objektspeicher oder einem CDN kommen.
- **Web-Admin** für Einrichtung, Uploads und Benutzer; Handys verbinden sich per QR-Code.

## Betrieb mit Docker

Das Image `ghcr.io/joveptesg/berth` wird für amd64 und arm64 gebaut (Raspberry Pi, ARM-NAS).

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

Öffne dann die Adresse des Servers im Browser: `http://<server>:8080`, eine IP mit Port oder die
Domain, die dir dein Hoster zugewiesen hat (z. B. `berth-….sslip.io`). Gib den Einrichtungscode aus der Log-Zeile
`server is not set up: … enter the setup code K7P2-9QXM` ein; bis die Einrichtung abgeschlossen ist,
ändert sich der Code bei jedem Neustart. Die geöffnete Adresse wird zur Heimnetz- oder Internetadresse in den
QR-Codes für Handys (Einstellungen → Server). Wenn der Hoster seine Domain an den Container übergibt, trag sie in
`BERTH_WAN_URL` ein — dann zeigt das Log einen fertigen Link.

In `./data` liegen die Datenbank, ihre Backups, der lokale Speicher und `secret.key`.

> [!IMPORTANT]
> Bewahre eine Kopie von `secret.key` getrennt von den Backups auf: Ohne sie lassen sich die Speicherschlüssel in der Datenbank nicht lesen.

### Image-Tags

| Tag | Folgt |
|---|---|
| `1.0.1` | genau dieser Version, ändert sich nie |
| `1.0` | den Fehlerkorrekturen von 1.0 |
| `1` | allen 1.x-Releases (verwendet in `docker-compose.yml`) |
| `latest` | dem neuesten Release, einschließlich Major-Versionen |

Um bei einer Version zu bleiben, trag sie in `docker-compose.yml` ein, z. B. `image: ghcr.io/joveptesg/berth:1.0.1`.
Alle Versionen findest du unter [Releases](https://github.com/joveptesg/berth/releases).

### Ohne Compose

Für reines Docker oder ein NAS, das nach einem Image-Namen fragt (Synology, Unraid, CasaOS):

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

In der Oberfläche des NAS: Image `ghcr.io/joveptesg/berth:1`, Port `8080`, ein Ordner, eingebunden unter `/data`.

Zum Aktualisieren führe `docker pull ghcr.io/joveptesg/berth:1` aus, entferne den Container mit `docker rm -f berth`
und starte ihn mit demselben `docker run`-Befehl neu; die Daten bleiben in `./data`.

### Aktualisieren und Zurücksetzen

```sh
docker compose pull && docker compose up -d
```

Der Tag `1` folgt den 1.x-Releases; für eine neue Major-Version muss der Tag in
`docker-compose.yml` geändert werden. Verwende `latest` nicht mit automatischen Updatern wie Watchtower.

Bevor die neue Version das Datenbankschema ändert, legt sie eine Kopie in `./data/backups` ab
(die letzten 3 werden aufbewahrt; sie stehen auch im Admin unter Einstellungen → Backup). Um zurückzugehen,
leg in `docker-compose.yml` die vorherige Version fest (z. B. `image: ghcr.io/joveptesg/berth:1.0.0`)
und stelle die Kopie wieder her, die vor dem Update angelegt wurde:

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### Zugriff aus dem Internet

Berth spricht einfaches HTTP. Zu Hause reicht das; für den Zugriff von außen stell einen Reverse Proxy
mit HTTPS davor, z. B. [Caddy](https://caddyserver.com) mit einer `Caddyfile`:

```
books.example.com {
	reverse_proxy localhost:8080
}
```

Trag dann `https://books.example.com` im Admin unter Einstellungen → Adresse im Internet ein, damit
die QR-Codes für Handys sie enthalten.

> [!WARNING]
> Lass `./data` auf einer lokalen Festplatte. SQLite auf einer Netzwerkfreigabe (NFS, SMB) kann die Datenbank beschädigen.

### Aus dem Quellcode bauen

Entkommentiere `build: .` in `docker-compose.yml` in einem Klon dieses Repositorys und führe
`docker compose up -d --build` aus.

### Backups und Wiederherstellung

- Unter Einstellungen → Backup im Admin lädst du eine Kopie der Datenbank herunter und stellst eine wieder her: aus einem
  automatischen Backup oder aus einer hochgeladenen Datei. Die aktuelle Datenbank wird vorher in den automatischen Backups
  gesichert, sodass sich eine Wiederherstellung auf dieselbe Weise rückgängig machen lässt.
- Über die Konsole (bei gestopptem Server): `berth admin restore <file>`.

### Logs

Das Log hält die Einrichtung fest, Anmeldungen (fehlgeschlagene mit der Adresse), verbundene Handys, Benutzer,
Änderungen an Zugriffsrechten und Einstellungen, Uploads und Bücher. Passwörter und Tokens landen nie darin. Jede HTTP-Anfrage
wird ebenfalls protokolliert, wenn in der Umgebung des Containers `RUST_LOG=info,tower_http=debug` gesetzt ist.

## Aufbau des Repositorys

| Pfad | Was es ist |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth` — die Server-Binärdatei |
| [`crates/berth-server`](crates/berth-server) | HTTP-Server: JSON-API v1 und der Web-Admin auf Basis von axum |
| [`crates/berth-core`](crates/berth-core) | Zentrale Domänentypen |
| [`openapi.json`](openapi.json) | API-Vertrag; `cargo test` schlägt fehl, wenn sich die API ändert |

## Mitwirken

Beiträge sind willkommen. Lies zuerst [CONTRIBUTING.md](CONTRIBUTING.md): Commits werden nach dem DCO abgezeichnet (Sign-off) und folgen Conventional Commits.

## Sicherheit

Bitte melde Sicherheitslücken nicht in öffentlichen Issues — siehe [SECURITY.md](SECURITY.md).

## Lizenz

[Apache License 2.0](LICENSE)
