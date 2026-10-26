<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth: servidor de audiolibros autoalojado" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <a href="README.de.md">Deutsch</a> · <b>Español</b> · <a href="README.fr.md">Français</a> · <a href="README.it.md">Italiano</a> · <a href="README.pt-BR.md">Português (Brasil)</a> · <a href="README.zh-Hans.md">简体中文</a> · <a href="README.ja.md">日本語</a> · <a href="README.ko.md">한국어</a>
</p>

Berth es un servidor de audiolibros autoalojado para ti y tu familia. Sube libros desde el panel web, guarda los archivos en el almacenamiento que quieras y escúchalos en la app Berth para Android e iOS.

## Por qué Berth

- **La base de datos es la fuente de verdad**: los libros son entidades, no carpetas escaneadas.
- **Almacenamiento intercambiable**: disco local, cualquier servicio compatible con S3 (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage) y Google Drive.
- **La entrega, separada del almacenamiento**: el servidor autoriza y los datos pueden llegar directamente desde el almacenamiento de objetos o una CDN mediante enlaces firmados de corta duración.
- **Panel web** para la configuración, la subida de libros y los usuarios; los teléfonos se conectan con un código QR.

## Ejecución con Docker

La imagen `ghcr.io/joveptesg/berth` se compila para amd64 y arm64 (Raspberry Pi, NAS con ARM).

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

Después abre la dirección del servidor en un navegador: `http://<server>:8080`, una IP con puerto o el
dominio que te asignó tu proveedor de alojamiento (p. ej., `berth-….sslip.io`). Introduce el código de configuración de la línea del registro
`server is not set up: … enter the setup code K7P2-9QXM`; el código cambia con cada reinicio
hasta que termines la configuración. La dirección que abriste pasa a ser la dirección de casa o de internet en los códigos QR
para los teléfonos (Ajustes → Servidor). Si el proveedor pasa su dominio al contenedor, indícalo en
`BERTH_WAN_URL` y el registro mostrará un enlace listo para usar.

`./data` contiene la base de datos, sus copias de seguridad, el almacenamiento local y `secret.key`.

> [!IMPORTANT]
> Guarda una copia de `secret.key` aparte de las copias de seguridad: sin ella no se pueden leer las claves de almacenamiento de la base de datos.

### Etiquetas de la imagen

| Etiqueta | Sigue |
|---|---|
| `1.0.1` | exactamente esta versión, nunca cambia |
| `1.0` | las correcciones de la 1.0 |
| `1` | todas las versiones 1.x (la que usa `docker-compose.yml`) |
| `latest` | la versión más reciente, incluidas las versiones mayores |

Para quedarte en una versión, fíjala en `docker-compose.yml`, p. ej., `image: ghcr.io/joveptesg/berth:1.0.1`.
Todas las versiones están en [Releases](https://github.com/joveptesg/berth/releases).

### Sin Compose

Para Docker a secas o un NAS que pide el nombre de una imagen (Synology, Unraid, CasaOS):

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

En la interfaz del NAS: imagen `ghcr.io/joveptesg/berth:1`, puerto `8080` y una carpeta montada en `/data`.

Para actualizar, ejecuta `docker pull ghcr.io/joveptesg/berth:1`, elimina el contenedor con `docker rm -f berth`
y vuelve a iniciarlo con el mismo comando `docker run`; los datos se quedan en `./data`.

### Actualizar y volver atrás

```sh
docker compose pull && docker compose up -d
```

La etiqueta `1` sigue las versiones 1.x; para una nueva versión mayor hay que cambiar la etiqueta en
`docker-compose.yml`. No uses `latest` con actualizadores automáticos como Watchtower.

Antes de cambiar el esquema de la base de datos, la nueva versión guarda una copia en `./data/backups`
(se conservan las 3 últimas; también aparecen en Ajustes → Copia de seguridad, en el panel). Para volver atrás,
fija la versión anterior en `docker-compose.yml` (p. ej., `image: ghcr.io/joveptesg/berth:1.0.0`)
y restaura la copia hecha antes de la actualización:

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### Acceso desde internet

Berth sirve HTTP simple. En casa basta con eso; para acceder desde fuera, pon delante un proxy inverso
con HTTPS, p. ej., [Caddy](https://caddyserver.com) con un `Caddyfile`:

```
books.example.com {
	reverse_proxy localhost:8080
}
```

Luego introduce `https://books.example.com` en el panel, en Ajustes → Dirección en internet, para que
los códigos QR de los teléfonos la incluyan.

> [!WARNING]
> Mantén `./data` en un disco local. SQLite en una carpeta de red compartida (NFS, SMB) puede dañar la base de datos.

### Compilar desde el código fuente

Descomenta `build: .` en `docker-compose.yml` dentro de un clon de este repositorio y ejecuta
`docker compose up -d --build`.

### Copias de seguridad y restauración

- En el panel, Ajustes → Copia de seguridad descarga una copia de la base de datos y restaura una: desde una
  copia automática o desde un archivo subido. Antes se guarda la base de datos actual en las copias automáticas,
  así que una restauración se puede deshacer de la misma forma.
- Desde la consola (con el servidor detenido): `berth admin restore <file>`.

### Registros

El registro anota la configuración, los inicios de sesión (los fallidos, con la dirección), los teléfonos vinculados, los usuarios,
los cambios de acceso y de ajustes, las subidas y los libros. Las contraseñas y los tokens nunca se registran. Cada petición
HTTP también se registra con `RUST_LOG=info,tower_http=debug` en el entorno del contenedor.

## Estructura del repositorio

| Ruta | Qué es |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth`: el binario del servidor |
| [`crates/berth-server`](crates/berth-server) | Servidor HTTP: API JSON v1 y el panel web sobre axum |
| [`crates/berth-core`](crates/berth-core) | Tipos básicos del dominio |
| [`openapi.json`](openapi.json) | Contrato de la API; `cargo test` falla cuando la API cambia |

## Contribuir

Las contribuciones son bienvenidas. Lee primero [CONTRIBUTING.md](CONTRIBUTING.md): los commits se firman conforme al DCO y siguen Conventional Commits.

## Seguridad

No informes de vulnerabilidades en issues públicos; consulta [SECURITY.md](SECURITY.md).

## Licencia

[Apache License 2.0](LICENSE)
