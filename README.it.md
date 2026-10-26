<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — server di audiolibri self-hosted" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <a href="README.de.md">Deutsch</a> · <a href="README.es.md">Español</a> · <a href="README.fr.md">Français</a> · <b>Italiano</b> · <a href="README.pt-BR.md">Português (Brasil)</a> · <a href="README.zh-Hans.md">简体中文</a> · <a href="README.ja.md">日本語</a> · <a href="README.ko.md">한국어</a>
</p>

Berth è un server di audiolibri self-hosted per te e la tua famiglia. Carica i libri dal pannello web, conserva i file in qualsiasi archivio e ascoltali nell'app Berth per Android e iOS.

## Perché Berth

- **Il database è la fonte di verità**: i libri sono entità, non cartelle scansionate.
- **Archivi intercambiabili**: disco locale, qualsiasi servizio compatibile con S3 (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **Distribuzione separata dall'archiviazione**: il server autorizza l'accesso, mentre i byte possono arrivare direttamente dall'object storage o da una CDN tramite link firmati a breve scadenza.
- **Pannello web** per configurazione, caricamento dei libri e utenti; i telefoni si collegano con un codice QR.

## Avvio con Docker

L'immagine `ghcr.io/joveptesg/berth` è compilata per amd64 e arm64 (Raspberry Pi, NAS ARM).

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

Poi apri l'indirizzo del server in un browser: `http://<server>:8080`, un IP con la porta o il
dominio assegnato dal tuo hosting (es. `berth-….sslip.io`). Inserisci il codice di configurazione dalla riga del log
`server is not set up: … enter the setup code K7P2-9QXM`; il codice cambia a ogni riavvio
finché la configurazione non è completata. L'indirizzo che hai aperto diventa l'indirizzo di casa o quello Internet
nei codici QR per i telefoni (Impostazioni → Server). Se l'hosting passa il proprio dominio al container, indicalo in
`BERTH_WAN_URL` e il log mostrerà un link pronto.

`./data` contiene il database, i suoi backup, l'archivio locale e `secret.key`.

> [!IMPORTANT]
> Conserva una copia di `secret.key` separata dai backup: senza di essa le chiavi degli archivi nel database non si possono leggere.

### Tag dell'immagine

| Tag | Segue |
|---|---|
| `1.0.1` | esattamente questa versione, non cambia mai |
| `1.0` | le correzioni della 1.0 |
| `1` | tutte le release 1.x (usato in `docker-compose.yml`) |
| `latest` | la release più recente, comprese le versioni major |

Per restare su una versione, indicala in `docker-compose.yml`, es. `image: ghcr.io/joveptesg/berth:1.0.1`.
Tutte le versioni sono elencate in [Releases](https://github.com/joveptesg/berth/releases).

### Senza Compose

Per Docker semplice o un NAS che chiede il nome di un'immagine (Synology, Unraid, CasaOS):

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

Nell'interfaccia del NAS: immagine `ghcr.io/joveptesg/berth:1`, porta `8080`, una cartella montata su `/data`.

Per aggiornare, esegui `docker pull ghcr.io/joveptesg/berth:1`, rimuovi il container con `docker rm -f berth`
e riavvialo con lo stesso comando `docker run`; i dati restano in `./data`.

### Aggiornamento e ritorno alla versione precedente

```sh
docker compose pull && docker compose up -d
```

Il tag `1` segue le release 1.x; per una nuova versione major bisogna cambiare il tag in
`docker-compose.yml`. Evita `latest` con strumenti di aggiornamento automatico come Watchtower.

Prima di modificare lo schema del database, la nuova versione ne salva una copia in `./data/backups`
(vengono conservate le ultime 3; sono elencate anche nel pannello web, in Impostazioni → Backup). Per tornare indietro,
fissa la versione precedente in `docker-compose.yml` (es. `image: ghcr.io/joveptesg/berth:1.0.0`)
e ripristina la copia fatta prima dell'aggiornamento:

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### Accesso da Internet

Berth serve HTTP semplice. A casa basta; per l'accesso dall'esterno metti davanti un reverse proxy
con HTTPS, per esempio [Caddy](https://caddyserver.com) con un `Caddyfile`:

```
books.example.com {
	reverse_proxy localhost:8080
}
```

Poi inserisci `https://books.example.com` nel pannello web, in Impostazioni → Indirizzo Internet, così
i codici QR per i telefoni lo includeranno.

> [!WARNING]
> Tieni `./data` su un disco locale. SQLite su una cartella di rete (NFS, SMB) può danneggiare il database.

### Compilare dai sorgenti

In un clone di questo repository, togli il commento a `build: .` in `docker-compose.yml` ed esegui
`docker compose up -d --build`.

### Backup e ripristino

- Nel pannello web, Impostazioni → Backup scarica una copia del database e ne ripristina una: da un
  backup automatico o da un file caricato. Il database attuale viene prima salvato nei backup automatici,
  quindi un ripristino si può annullare allo stesso modo.
- Dalla console (server fermo): `berth admin restore <file>`.

### Log

Il log registra la configurazione, gli accessi (quelli falliti con l'indirizzo), i telefoni associati, gli utenti,
le modifiche ad accessi e impostazioni, i caricamenti e i libri. Password e token non ci finiscono mai. Anche ogni
richiesta HTTP viene registrata con `RUST_LOG=info,tower_http=debug` nell'ambiente del container.

## Struttura del repository

| Percorso | Cos'è |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth` — l'eseguibile del server |
| [`crates/berth-server`](crates/berth-server) | Server HTTP: API JSON v1 e pannello web su axum |
| [`crates/berth-core`](crates/berth-core) | Tipi di dominio di base |
| [`openapi.json`](openapi.json) | Contratto dell'API; `cargo test` fallisce quando l'API cambia |

## Contribuire

I contributi sono benvenuti. Leggi prima [CONTRIBUTING.md](CONTRIBUTING.md): i commit sono firmati secondo il DCO e seguono Conventional Commits.

## Sicurezza

Non segnalare vulnerabilità nelle issue pubbliche: consulta [SECURITY.md](SECURITY.md).

## Licenza

[Apache License 2.0](LICENSE)
