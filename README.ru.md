<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — self-hosted сервер аудиокниг" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <b>Русский</b>
</p>

Berth — сервер аудиокниг, который вы разворачиваете у себя, для вас и вашей семьи. Книги загружаются через веб-админку, файлы лежат в любом хранилище, а слушать их можно в приложении Berth для Android и iOS.

## Почему Berth

- **Источник правды — база данных**: книги — это сущности, а не просканированные папки.
- **Подключаемые хранилища**: локальный диск, любой S3-совместимый сервис (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **Раздача отделена от хранения**: сервер проверяет доступ, а байты могут идти прямо из объектного хранилища или CDN по короткоживущим подписанным ссылкам.
- **Веб-админка** для настройки, загрузки книг и пользователей; телефоны подключаются по QR-коду.

## Запуск в Docker

Образ `ghcr.io/joveptesg/berth` собран для amd64 и arm64 (Raspberry Pi, NAS на ARM).

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, данные в ./data
docker compose logs berth         # одноразовая ссылка для настройки — в логе
```

В `./data` лежат база данных, её резервные копии, локальное хранилище и `secret.key`.

> [!IMPORTANT]
> Храните копию `secret.key` отдельно от резервных копий: без него ключи хранилищ в базе не прочитать.

### Теги образа

| Тег | Что получаете |
|---|---|
| `1.0.1` | ровно эту версию, тег не меняется |
| `1.0` | исправления версии 1.0 |
| `1` | все релизы 1.x (стоит в `docker-compose.yml`) |
| `latest` | самый новый релиз, включая мажорные версии |

Чтобы остаться на одной версии, укажите её в `docker-compose.yml`, например `image: ghcr.io/joveptesg/berth:1.0.1`.
Все версии — в [Releases](https://github.com/joveptesg/berth/releases).

### Без Compose

Для обычного Docker или NAS, который просит имя образа (Synology, Unraid, CasaOS):

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # одноразовая ссылка для настройки — в логе
```

В интерфейсе NAS: образ `ghcr.io/joveptesg/berth:1`, порт `8080`, папка, подключённая к `/data`.

Чтобы обновиться, выполните `docker pull ghcr.io/joveptesg/berth:1`, удалите контейнер командой
`docker rm -f berth` и запустите его той же командой `docker run`; данные останутся в `./data`.

### Обновление и откат

```sh
docker compose pull && docker compose up -d
```

Тег `1` следует за релизами 1.x; для новой мажорной версии тег в `docker-compose.yml` нужно
сменить вручную. Не используйте `latest` с автообновлялками вроде Watchtower.

Перед изменением схемы базы новая версия сохраняет копию в `./data/backups`
(хранятся последние 3; они же видны в админке в разделе Settings → Backup). Чтобы откатиться,
укажите предыдущую версию в `docker-compose.yml` (например, `image: ghcr.io/joveptesg/berth:1.0.0`)
и восстановите копию, сделанную перед обновлением:

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### Доступ из интернета

Berth отдаёт обычный HTTP. Дома этого достаточно; для доступа снаружи поставьте перед ним
reverse proxy с HTTPS, например [Caddy](https://caddyserver.com) с таким `Caddyfile`:

```
books.example.com {
	reverse_proxy localhost:8080
}
```

Затем укажите `https://books.example.com` в админке в поле «Адрес в интернете» (Settings),
чтобы он попал в QR-коды для телефонов.

> [!WARNING]
> Держите `./data` на локальном диске. SQLite на сетевой папке (NFS, SMB) может повредить базу.

### Сборка из исходников

В клоне репозитория раскомментируйте `build: .` в `docker-compose.yml` и запустите
`docker compose up -d --build`.

### Резервные копии и восстановление

- В админке Settings → Backup скачивает копию базы и восстанавливает её — из автоматической
  копии или из загруженного файла. Текущая база сначала сохраняется в автоматические копии,
  так что восстановление можно отменить тем же способом.
- Из консоли (сервер остановлен): `berth admin restore <file>`.

## Структура репозитория

| Путь | Что это |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth` — исполняемый файл сервера |
| [`crates/berth-server`](crates/berth-server) | HTTP-сервер: JSON API v1 и веб-админка на axum |
| [`crates/berth-core`](crates/berth-core) | Базовые доменные типы |
| [`openapi.json`](openapi.json) | Контракт API; `cargo test` падает, если API изменился |

## Участие в разработке

Будем рады помощи. Сначала прочитайте [CONTRIBUTING.md](CONTRIBUTING.md): коммиты подписываются по DCO и следуют Conventional Commits.

## Безопасность

Не сообщайте об уязвимостях в публичных issue — см. [SECURITY.md](SECURITY.md).

## Лицензия

[Apache License 2.0](LICENSE)
