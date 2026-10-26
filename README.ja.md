<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — セルフホスト型オーディオブックサーバー" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <a href="README.de.md">Deutsch</a> · <a href="README.es.md">Español</a> · <a href="README.fr.md">Français</a> · <a href="README.it.md">Italiano</a> · <a href="README.pt-BR.md">Português (Brasil)</a> · <a href="README.zh-Hans.md">简体中文</a> · <b>日本語</b> · <a href="README.ko.md">한국어</a>
</p>

Berthは、あなたとご家族のためのセルフホスト型オーディオブックサーバーです。Web管理画面から書籍をアップロードし、ファイルは好きなストレージに保存して、Android・iOS向けのBerthアプリで聴けます。

## Berthを選ぶ理由

- **データベースが唯一の情報源**：書籍はスキャンしたフォルダではなく、独立したエンティティとして管理されます。
- **ストレージを自由に選べる**：ローカルディスク、任意のS3互換サービス（AWS、Cloudflare R2、Backblaze B2、MinIO、Yandex Object Storage）、Google Driveに対応しています。
- **配信と保存を分離**：サーバーはアクセスの認可を行い、音声データは有効期限の短い署名付きURLでオブジェクトストレージやCDNから直接配信できます。
- **Web管理画面**でセットアップ、アップロード、ユーザー管理を行えます。スマートフォンはQRコードで接続します。

## Dockerで起動する

イメージ`ghcr.io/joveptesg/berth`は、amd64とarm64（Raspberry Pi、ARMベースのNAS）向けにビルドされています。

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

次に、ブラウザでサーバーのアドレスを開きます。`http://<server>:8080`のようなIPとポート、またはホスティングサービスが割り当てたドメイン（例：`berth-….sslip.io`）です。ログの行`server is not set up: … enter the setup code K7P2-9QXM`に表示されたセットアップコードを入力してください。セットアップが完了するまで、コードは再起動のたびに変わります。開いたアドレスは、スマートフォン用QRコードのホームネットワークのアドレスまたはインターネットのアドレスになります（設定 → サーバー）。ホスティングサービスがコンテナにドメインを渡す場合は、それを`BERTH_WAN_URL`に設定すると、ログにそのまま開けるリンクが表示されます。

`./data`には、データベース、そのバックアップ、ローカルストレージ、`secret.key`が保存されます。

> [!IMPORTANT]
> `secret.key`のコピーはバックアップとは別の場所に保管してください。これがないと、データベース内のストレージのキーを読み取れません。

### イメージのタグ

| タグ | 追従する対象 |
|---|---|
| `1.0.1` | このバージョンそのもの。変わることはありません |
| `1.0` | 1.0の修正版 |
| `1` | すべての1.xリリース（`docker-compose.yml`で使用） |
| `latest` | メジャーバージョンを含む最新リリース |

1つのバージョンに固定するには、`docker-compose.yml`で指定します。例：`image: ghcr.io/joveptesg/berth:1.0.1`。
すべてのバージョンは[Releases](https://github.com/joveptesg/berth/releases)に掲載されています。

### Composeを使わない場合

通常のDocker、またはイメージ名を入力するタイプのNAS（Synology、Unraid、CasaOS）の場合：

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

NASの画面では、イメージに`ghcr.io/joveptesg/berth:1`、ポートに`8080`を指定し、フォルダを`/data`にマウントします。

アップデートするには、`docker pull ghcr.io/joveptesg/berth:1`を実行し、`docker rm -f berth`でコンテナを削除してから、同じ`docker run`コマンドで再び起動します。データは`./data`に残ります。

### アップデートとロールバック

```sh
docker compose pull && docker compose up -d
```

`1`タグは1.xリリースに追従します。新しいメジャーバージョンに移行するには、`docker-compose.yml`のタグを変更する必要があります。Watchtowerなどの自動アップデートツールでは`latest`を使わないでください。

新しいバージョンは、データベーススキーマを変更する前に`./data/backups`へコピーを保存します（最新の3件が保持され、管理画面の「設定 → バックアップ」にも表示されます）。ロールバックするには、`docker-compose.yml`で以前のバージョンを固定し（例：`image: ghcr.io/joveptesg/berth:1.0.0`）、アップデート前に作成されたコピーを復元します。

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### インターネットからのアクセス

Berthは通常のHTTPで配信します。自宅内ならそれで十分ですが、外部からアクセスする場合は、HTTPSに対応したリバースプロキシを前段に置いてください。たとえば[Caddy](https://caddyserver.com)なら、次のような`Caddyfile`を使います。

```
books.example.com {
	reverse_proxy localhost:8080
}
```

その後、管理画面の「設定 → インターネットのアドレス」に`https://books.example.com`を入力すると、スマートフォン用のQRコードにこのアドレスが含まれるようになります。

> [!WARNING]
> `./data`はローカルディスクに置いてください。ネットワーク共有（NFS、SMB）上のSQLiteは、データベースを破損させるおそれがあります。

### ソースからビルドする

このリポジトリのクローン内で、`docker-compose.yml`の`build: .`のコメントを外し、`docker compose up -d --build`を実行します。

### バックアップと復元

- 管理画面の「設定 → バックアップ」では、データベースのコピーをダウンロードしたり、自動バックアップやアップロードしたファイルから復元したりできます。復元の前に現在のデータベースが自動バックアップに保存されるため、同じ方法で復元を取り消せます。
- コンソールから（サーバー停止中）：`berth admin restore <file>`。

### ログ

ログには、セットアップ、ログイン（失敗したものは接続元アドレス付き）、スマートフォンのペアリング、ユーザー・アクセス権・設定の変更、アップロード、書籍の操作が記録されます。パスワードやトークンが記録されることはありません。コンテナの環境変数に`RUST_LOG=info,tower_http=debug`を設定すると、すべてのHTTPリクエストも記録されます。

## リポジトリの構成

| パス | 内容 |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth`：サーバーの実行ファイル |
| [`crates/berth-server`](crates/berth-server) | HTTPサーバー：axumで実装したJSON API v1とWeb管理画面 |
| [`crates/berth-core`](crates/berth-core) | コアのドメイン型 |
| [`openapi.json`](openapi.json) | APIの仕様。APIが変わると`cargo test`が失敗します |

## コントリビュート

コントリビューションを歓迎します。まず[CONTRIBUTING.md](CONTRIBUTING.md)をお読みください。コミットにはDCOに基づくサインオフが必要で、Conventional Commitsに従います。

## セキュリティ

脆弱性は公開のissueで報告しないでください。詳しくは[SECURITY.md](SECURITY.md)をご覧ください。

## ライセンス

[Apache License 2.0](LICENSE)
