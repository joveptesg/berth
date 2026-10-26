<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — servidor de audiolivros self-hosted" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <a href="README.de.md">Deutsch</a> · <a href="README.es.md">Español</a> · <a href="README.fr.md">Français</a> · <a href="README.it.md">Italiano</a> · <b>Português (Brasil)</b> · <a href="README.zh-Hans.md">简体中文</a> · <a href="README.ja.md">日本語</a> · <a href="README.ko.md">한국어</a>
</p>

O Berth é um servidor de audiolivros self-hosted para você e sua família. Envie livros pelo painel web, guarde os arquivos em qualquer armazenamento e ouça no app Berth para Android e iOS.

## Por que o Berth

- **O banco de dados é a fonte da verdade** — livros são entidades, não pastas varridas por um scanner.
- **Armazenamento plugável** — disco local, qualquer serviço compatível com S3 (AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **Entrega separada do armazenamento** — o servidor autoriza, e os bytes podem vir direto do armazenamento de objetos ou de uma CDN por links assinados de curta duração.
- **Painel web** para configuração, envio de livros e usuários; os celulares se conectam por QR code.

## Rodando com Docker

A imagem `ghcr.io/joveptesg/berth` é compilada para amd64 e arm64 (Raspberry Pi, NAS com ARM).

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

Depois abra o endereço do servidor no navegador: `http://<server>:8080`, um IP com porta ou o
domínio atribuído pela sua hospedagem (por exemplo, `berth-….sslip.io`). Informe o código de configuração da linha do log
`server is not set up: … enter the setup code K7P2-9QXM`; o código muda a cada reinicialização
até a configuração ser concluída. O endereço que você abriu passa a ser o endereço doméstico ou da internet nos
QR codes para celulares (Configurações → Servidor). Se a hospedagem repassa o próprio domínio ao contêiner, coloque-o em
`BERTH_WAN_URL` e o log mostra um link pronto.

`./data` guarda o banco de dados, os backups dele, o armazenamento local e o `secret.key`.

> [!IMPORTANT]
> Guarde uma cópia do `secret.key` separada dos backups: sem ele, as chaves dos armazenamentos no banco de dados não podem ser lidas.

### Tags da imagem

| Tag | Acompanha |
|---|---|
| `1.0.1` | exatamente esta versão, nunca muda |
| `1.0` | correções da 1.0 |
| `1` | todos os lançamentos 1.x (usada no `docker-compose.yml`) |
| `latest` | o lançamento mais recente, incluindo versões principais |

Para ficar em uma única versão, defina-a no `docker-compose.yml`, por exemplo `image: ghcr.io/joveptesg/berth:1.0.1`.
Todas as versões estão listadas em [Releases](https://github.com/joveptesg/berth/releases).

### Sem Compose

Para Docker puro ou um NAS que pede o nome da imagem (Synology, Unraid, CasaOS):

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

Na interface do NAS: imagem `ghcr.io/joveptesg/berth:1`, porta `8080`, uma pasta montada em `/data`.

Para atualizar, execute `docker pull ghcr.io/joveptesg/berth:1`, remova o contêiner com `docker rm -f berth`
e inicie-o de novo com o mesmo comando `docker run`; os dados continuam em `./data`.

### Atualização e reversão

```sh
docker compose pull && docker compose up -d
```

A tag `1` acompanha os lançamentos 1.x; uma nova versão principal exige trocar a tag no
`docker-compose.yml`. Evite `latest` com atualizadores automáticos como o Watchtower.

Antes de alterar o esquema do banco de dados, a nova versão salva uma cópia em `./data/backups`
(as 3 últimas são mantidas; elas também aparecem em Configurações → Backup no painel). Para voltar à versão anterior,
fixe-a no `docker-compose.yml` (por exemplo, `image: ghcr.io/joveptesg/berth:1.0.0`)
e restaure a cópia feita antes da atualização:

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### Acesso pela internet

O Berth serve HTTP simples. Em casa isso basta; para acessar de fora, coloque na frente um proxy reverso
com HTTPS, por exemplo o [Caddy](https://caddyserver.com) com um `Caddyfile`:

```
books.example.com {
	reverse_proxy localhost:8080
}
```

Depois informe `https://books.example.com` no painel em Configurações → Endereço na internet, para que os
QR codes para celulares o incluam.

> [!WARNING]
> Mantenha `./data` em um disco local. SQLite em um compartilhamento de rede (NFS, SMB) pode corromper o banco de dados.

### Compilando a partir do código-fonte

Descomente `build: .` no `docker-compose.yml` dentro de um clone deste repositório e execute
`docker compose up -d --build`.

### Backups e restauração

- Configurações → Backup no painel baixa uma cópia do banco de dados e restaura uma: de um
  backup automático ou de um arquivo enviado. O banco de dados atual é salvo antes nos backups automáticos,
  então uma restauração pode ser desfeita do mesmo jeito.
- Pelo console (com o servidor parado): `berth admin restore <file>`.

### Logs

O log registra a configuração, os logins (os que falharam, com o endereço), os celulares pareados, os usuários,
as mudanças de acesso e de configurações, os envios e os livros. Senhas e tokens nunca vão para lá. Cada requisição
HTTP também é registrada com `RUST_LOG=info,tower_http=debug` no ambiente do contêiner.

## Estrutura do repositório

| Caminho | O que é |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth` — o binário do servidor |
| [`crates/berth-server`](crates/berth-server) | Servidor HTTP: JSON API v1 e o painel web em axum |
| [`crates/berth-core`](crates/berth-core) | Tipos centrais do domínio |
| [`openapi.json`](openapi.json) | Contrato da API; `cargo test` falha quando a API muda |

## Como contribuir

Contribuições são bem-vindas. Leia primeiro o [CONTRIBUTING.md](CONTRIBUTING.md): os commits levam sign-off sob o DCO e seguem o Conventional Commits.

## Segurança

Por favor, não relate vulnerabilidades em issues públicas — veja o [SECURITY.md](SECURITY.md).

## Licença

[Apache License 2.0](LICENSE)
