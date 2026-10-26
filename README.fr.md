<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — serveur de livres audio auto-hébergé" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <a href="README.de.md">Deutsch</a> · <a href="README.es.md">Español</a> · <b>Français</b> · <a href="README.it.md">Italiano</a> · <a href="README.pt-BR.md">Português (Brasil)</a> · <a href="README.zh-Hans.md">简体中文</a> · <a href="README.ja.md">日本語</a> · <a href="README.ko.md">한국어</a>
</p>

Berth est un serveur de livres audio auto-hébergé pour vous et votre famille. Importez vos livres via l'administration web, conservez les fichiers sur le stockage de votre choix et écoutez-les dans l'application Berth pour Android et iOS.

## Pourquoi Berth

- **La base de données fait foi** : les livres sont des entités, pas des dossiers analysés.
- **Stockage modulaire** : disque local, n'importe quel service compatible S3 (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **Diffusion séparée du stockage** : le serveur autorise l'accès, et les octets peuvent venir directement du stockage objet ou d'un CDN via des liens signés à courte durée de vie.
- **Administration web** pour la configuration, l'import des livres et les utilisateurs ; les téléphones se connectent par QR code.

## Lancer avec Docker

L'image `ghcr.io/joveptesg/berth` est construite pour amd64 et arm64 (Raspberry Pi, NAS ARM).

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

Ouvrez ensuite l'adresse du serveur dans un navigateur : `http://<serveur>:8080`, une IP avec un port ou le
domaine attribué par votre hébergeur (par ex. `berth-….sslip.io`). Saisissez le code de configuration indiqué
dans la ligne du journal `server is not set up: … enter the setup code K7P2-9QXM` ; le code change à chaque
redémarrage tant que la configuration n'est pas terminée. L'adresse que vous avez ouverte devient l'adresse
domestique ou l'adresse sur Internet dans les QR codes des téléphones (Paramètres → Serveur). Si l'hébergeur
transmet son domaine au conteneur, indiquez-le dans `BERTH_WAN_URL` : le journal affichera alors un lien prêt à l'emploi.

`./data` contient la base de données, ses sauvegardes, le stockage local et `secret.key`.

> [!IMPORTANT]
> Conservez une copie de `secret.key` à part des sauvegardes : sans elle, les clés des stockages enregistrées dans la base sont illisibles.

### Tags de l'image

| Tag | Suit |
|---|---|
| `1.0.1` | exactement cette version, ne change jamais |
| `1.0` | les correctifs de la 1.0 |
| `1` | toutes les versions 1.x (utilisé dans `docker-compose.yml`) |
| `latest` | la version la plus récente, y compris les versions majeures |

Pour rester sur une version précise, indiquez-la dans `docker-compose.yml`, par ex. `image: ghcr.io/joveptesg/berth:1.0.1`.
Toutes les versions sont listées dans [Releases](https://github.com/joveptesg/berth/releases).

### Sans Compose

Pour Docker seul ou un NAS qui demande un nom d'image (Synology, Unraid, CasaOS) :

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

Dans l'interface d'un NAS : image `ghcr.io/joveptesg/berth:1`, port `8080`, un dossier monté sur `/data`.

Pour mettre à jour, lancez `docker pull ghcr.io/joveptesg/berth:1`, supprimez le conteneur avec `docker rm -f berth`
puis redémarrez-le avec la même commande `docker run` ; les données restent dans `./data`.

### Mise à jour et retour en arrière

```sh
docker compose pull && docker compose up -d
```

Le tag `1` suit les versions 1.x ; pour passer à une nouvelle version majeure, il faut modifier le tag dans
`docker-compose.yml`. Évitez `latest` avec des outils de mise à jour automatique comme Watchtower.

Avant de modifier le schéma de la base de données, la nouvelle version en enregistre une copie dans `./data/backups`
(les 3 dernières sont conservées ; elles apparaissent aussi dans l'administration, sous Paramètres → Sauvegarde). Pour revenir
en arrière, fixez la version précédente dans `docker-compose.yml` (par ex. `image: ghcr.io/joveptesg/berth:1.0.0`)
et restaurez la copie faite avant la mise à jour :

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### Accès depuis Internet

Berth sert du HTTP simple. À la maison, cela suffit ; pour un accès depuis l'extérieur, placez devant lui un reverse proxy
avec HTTPS, par exemple [Caddy](https://caddyserver.com) avec un `Caddyfile` :

```
books.example.com {
	reverse_proxy localhost:8080
}
```

Saisissez ensuite `https://books.example.com` dans l'administration, sous Paramètres → Adresse sur Internet, pour que
les QR codes des téléphones l'incluent.

> [!WARNING]
> Gardez `./data` sur un disque local. SQLite sur un partage réseau (NFS, SMB) peut corrompre la base de données.

### Compiler depuis les sources

Dans un clone de ce dépôt, décommentez `build: .` dans `docker-compose.yml` et lancez
`docker compose up -d --build`.

### Sauvegarde et restauration

- Dans l'administration, Paramètres → Sauvegarde permet de télécharger une copie de la base de données et d'en restaurer une :
  depuis une sauvegarde automatique ou un fichier importé. La base actuelle est d'abord enregistrée dans les sauvegardes
  automatiques, ce qui permet d'annuler une restauration de la même façon.
- Depuis la console (serveur arrêté) : `berth admin restore <file>`.

### Journaux

Le journal enregistre la configuration, les connexions (les échecs avec l'adresse), l'appairage des téléphones, les utilisateurs,
les modifications des accès et des paramètres, les imports et les livres. Les mots de passe et les jetons n'y figurent jamais.
Chaque requête HTTP est aussi journalisée avec `RUST_LOG=info,tower_http=debug` dans l'environnement du conteneur.

## Structure du dépôt

| Chemin | Rôle |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth` : l'exécutable du serveur |
| [`crates/berth-server`](crates/berth-server) | Serveur HTTP : API JSON v1 et administration web sur axum |
| [`crates/berth-core`](crates/berth-core) | Types du domaine de base |
| [`openapi.json`](openapi.json) | Contrat de l'API ; `cargo test` échoue quand l'API change |

## Contribuer

Les contributions sont les bienvenues. Lisez d'abord [CONTRIBUTING.md](CONTRIBUTING.md) : les commits sont signés selon le DCO et suivent Conventional Commits.

## Sécurité

Merci de ne pas signaler de vulnérabilités dans des issues publiques : consultez [SECURITY.md](SECURITY.md).

## Licence

[Apache License 2.0](LICENSE)
