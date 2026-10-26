<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — 自托管有声书服务器" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <a href="README.de.md">Deutsch</a> · <a href="README.es.md">Español</a> · <a href="README.fr.md">Français</a> · <a href="README.it.md">Italiano</a> · <a href="README.pt-BR.md">Português (Brasil)</a> · <b>简体中文</b> · <a href="README.ja.md">日本語</a> · <a href="README.ko.md">한국어</a>
</p>

Berth 是一款为你和家人打造的自托管有声书服务器。通过网页后台上传书籍，文件可存放在任意存储中，然后在 Android 和 iOS 版 Berth 应用中收听。

## 为什么选择 Berth

- **数据库是唯一的事实来源**：书籍是独立的实体，而不是扫描出来的文件夹。
- **可插拔存储**：本地磁盘、任意兼容 S3 的服务（AWS、Cloudflare R2、Backblaze B2、MinIO、Yandex Object Storage）、Google Drive。
- **分发与存储分离**：服务器负责授权，音频数据可以通过短时效签名链接直接从对象存储或 CDN 传输。
- **网页后台**用于设置服务器、上传书籍和管理用户；手机通过二维码连接。

## 使用 Docker 运行

镜像 `ghcr.io/joveptesg/berth` 提供 amd64 和 arm64 版本（Raspberry Pi、ARM 架构的 NAS）。

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

然后在浏览器中打开服务器地址：`http://<server>:8080`，可以是带端口的 IP，也可以是托管服务分配的域名（例如 `berth-….sslip.io`）。输入日志行 `server is not set up: … enter the setup code K7P2-9QXM` 中的设置码；在完成设置之前，每次重启设置码都会改变。你打开的这个地址会成为手机二维码中的家庭网络地址或互联网地址（设置 → 服务器）。如果托管服务会把它的域名传给容器，请将其填入 `BERTH_WAN_URL`，日志中就会显示可直接打开的链接。

`./data` 中存放数据库、数据库备份、本地存储和 `secret.key`。

> [!IMPORTANT]
> 请将 `secret.key` 的副本与备份分开保存：没有它，就无法读取数据库中的存储密钥。

### 镜像标签

| 标签 | 跟随 |
|---|---|
| `1.0.1` | 恰好这个版本，永不改变 |
| `1.0` | 1.0 的修复版本 |
| `1` | 所有 1.x 版本（`docker-compose.yml` 中使用的就是它） |
| `latest` | 最新版本，包括主版本升级 |

如需固定在某个版本，请在 `docker-compose.yml` 中指定，例如 `image: ghcr.io/joveptesg/berth:1.0.1`。
所有版本均列在 [Releases](https://github.com/joveptesg/berth/releases) 中。

### 不使用 Compose

适用于直接使用 Docker，或需要填写镜像名称的 NAS（Synology、Unraid、CasaOS）：

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

在 NAS 界面中：镜像填 `ghcr.io/joveptesg/berth:1`，端口填 `8080`，并将一个文件夹挂载到 `/data`。

更新时，先运行 `docker pull ghcr.io/joveptesg/berth:1`，再用 `docker rm -f berth` 删除容器，然后用同样的 `docker run` 命令重新启动；数据仍保留在 `./data` 中。

### 更新与回滚

```sh
docker compose pull && docker compose up -d
```

`1` 标签跟随 1.x 版本；升级到新的主版本需要修改 `docker-compose.yml` 中的标签。请勿将 `latest` 与 Watchtower 等自动更新工具一起使用。

在更改数据库结构之前，新版本会先在 `./data/backups` 中保存一份副本（保留最近 3 份；它们也会列在后台的“设置 → 备份”中）。如需回滚，请在 `docker-compose.yml` 中固定为上一个版本（例如 `image: ghcr.io/joveptesg/berth:1.0.0`），然后恢复更新前生成的副本：

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### 从互联网访问

Berth 提供的是普通 HTTP。在家中使用已经足够；如需从外部访问，请在前面放一个支持 HTTPS 的反向代理，例如使用 [Caddy](https://caddyserver.com) 及如下 `Caddyfile`：

```
books.example.com {
	reverse_proxy localhost:8080
}
```

然后在后台的“设置 → 互联网地址”中填入 `https://books.example.com`，这样手机二维码中就会包含这个地址。

> [!WARNING]
> 请将 `./data` 放在本地磁盘上。网络共享（NFS、SMB）上的 SQLite 可能会损坏数据库。

### 从源码构建

在本仓库的克隆中，取消 `docker-compose.yml` 里 `build: .` 的注释，然后运行 `docker compose up -d --build`。

### 备份与恢复

- 后台的“设置 → 备份”可以下载数据库副本，也可以从自动备份或上传的文件中恢复数据库。恢复前，当前数据库会先保存到自动备份中，因此可以用同样的方式撤销恢复。
- 通过命令行（服务器停止时）：`berth admin restore <file>`。

### 日志

日志会记录设置过程、登录（失败的登录会附带来源地址）、手机配对、用户、访问权限和设置的更改、上传以及书籍。密码和令牌绝不会写入日志。在容器环境变量中设置 `RUST_LOG=info,tower_http=debug` 后，每个 HTTP 请求也会被记录。

## 仓库结构

| 路径 | 说明 |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth`：服务器可执行文件 |
| [`crates/berth-server`](crates/berth-server) | HTTP 服务器：基于 axum 的 JSON API v1 和网页后台 |
| [`crates/berth-core`](crates/berth-core) | 核心领域类型 |
| [`openapi.json`](openapi.json) | API 契约；API 发生变化时 `cargo test` 会失败 |

## 参与贡献

欢迎贡献。请先阅读 [CONTRIBUTING.md](CONTRIBUTING.md)：提交需按 DCO 签署（sign-off），并遵循 Conventional Commits 规范。

## 安全

请不要在公开的 issue 中报告漏洞，详见 [SECURITY.md](SECURITY.md)。

## 许可证

[Apache License 2.0](LICENSE)
