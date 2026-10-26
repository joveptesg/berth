<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
    <img alt="Berth — 셀프 호스팅 오디오북 서버" src=".github/assets/banner-light.png" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/joveptesg/berth/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/joveptesg/berth/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-F2662B"></a>
  <img alt="Rust 1.99+" src="https://img.shields.io/badge/rust-1.99%2B-0E1014?logo=rust"></p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ru.md">Русский</a> · <a href="README.de.md">Deutsch</a> · <a href="README.es.md">Español</a> · <a href="README.fr.md">Français</a> · <a href="README.it.md">Italiano</a> · <a href="README.pt-BR.md">Português (Brasil)</a> · <a href="README.zh-Hans.md">简体中文</a> · <a href="README.ja.md">日本語</a> · <b>한국어</b>
</p>

Berth는 나와 가족을 위한 셀프 호스팅 오디오북 서버예요. 웹 관리자로 책을 업로드하고, 파일은 원하는 저장소에 두고, Android·iOS용 Berth 앱으로 들으세요.

## 왜 Berth인가요

- **데이터베이스가 기준이에요** — 책은 스캔한 폴더가 아니라 하나하나의 항목으로 관리돼요.
- **저장소를 골라 연결해요** — 로컬 디스크, 모든 S3 호환 서비스(AWS, Cloudflare R2, Backblaze B2, MinIO, Yandex Object Storage), Google Drive.
- **전송과 저장이 분리돼 있어요** — 서버는 권한만 확인하고, 데이터는 짧은 시간만 유효한 서명 링크를 통해 오브젝트 스토리지나 CDN에서 바로 전달될 수 있어요.
- **웹 관리자**에서 설정, 책 업로드, 사용자 관리를 하고, 휴대폰은 QR 코드로 연결해요.

## Docker로 실행하기

`ghcr.io/joveptesg/berth` 이미지는 amd64와 arm64(Raspberry Pi, ARM 기반 NAS)용으로 빌드돼요.

```sh
mkdir berth && cd berth
curl -fsSLO https://raw.githubusercontent.com/joveptesg/berth/main/docker-compose.yml
docker compose up -d              # http://localhost:8080, data in ./data
docker compose logs berth         # the setup code is in the log
```

그다음 브라우저에서 서버 주소를 여세요. `http://<server>:8080`처럼 IP와 포트를 쓰거나,
호스팅에서 받은 도메인(예: `berth-….sslip.io`)을 쓰면 돼요. 로그의
`server is not set up: … enter the setup code K7P2-9QXM` 줄에 있는 설정 코드를 입력하세요. 설정을 마칠 때까지
코드는 서버를 다시 시작할 때마다 바뀌어요. 이때 연 주소는 휴대폰용 QR 코드의 홈 네트워크 주소나
인터넷 주소가 돼요(설정 → 서버). 호스팅이 컨테이너에 자체 도메인을 넘겨준다면 그 도메인을
`BERTH_WAN_URL`에 넣으세요. 그러면 로그에 바로 쓸 수 있는 링크가 표시돼요.

`./data`에는 데이터베이스와 그 백업, 로컬 저장소, `secret.key`가 들어 있어요.

> [!IMPORTANT]
> `secret.key`의 사본은 백업과 따로 보관하세요. 이 키가 없으면 데이터베이스에 있는 저장소 키를 읽을 수 없어요.

### 이미지 태그

| 태그 | 따라가는 버전 |
|---|---|
| `1.0.1` | 정확히 이 버전, 절대 바뀌지 않음 |
| `1.0` | 1.0의 수정 버전 |
| `1` | 모든 1.x 릴리스(`docker-compose.yml`에서 사용) |
| `latest` | 메이저 버전을 포함한 최신 릴리스 |

한 버전에 머물고 싶다면 `docker-compose.yml`에 버전을 지정하세요. 예: `image: ghcr.io/joveptesg/berth:1.0.1`.
모든 버전은 [Releases](https://github.com/joveptesg/berth/releases)에서 볼 수 있어요.

### Compose 없이 실행하기

일반 Docker나 이미지 이름을 묻는 NAS(Synology, Unraid, CasaOS)에서는 이렇게 하세요.

```sh
docker run -d --name berth --restart unless-stopped \
  -p 8080:8080 -v "$PWD/data:/data" ghcr.io/joveptesg/berth:1
docker logs berth                 # the setup code is in the log
```

NAS 화면에서는 이미지 `ghcr.io/joveptesg/berth:1`, 포트 `8080`, `/data`에 마운트할 폴더를 지정하세요.

업데이트하려면 `docker pull ghcr.io/joveptesg/berth:1`을 실행하고, `docker rm -f berth`로 컨테이너를 삭제한 뒤
같은 `docker run` 명령으로 다시 시작하세요. 데이터는 `./data`에 그대로 남아요.

### 업데이트와 롤백

```sh
docker compose pull && docker compose up -d
```

`1` 태그는 1.x 릴리스를 따라가요. 새 메이저 버전으로 넘어가려면
`docker-compose.yml`의 태그를 바꿔야 해요. Watchtower 같은 자동 업데이트 도구와 함께 `latest`를 쓰지 마세요.

새 버전은 데이터베이스 스키마를 바꾸기 전에 `./data/backups`에 사본을 저장해요
(최근 3개가 보관되며, 관리자의 설정 → 백업에서도 볼 수 있어요). 롤백하려면
`docker-compose.yml`에 이전 버전을 고정하고(예: `image: ghcr.io/joveptesg/berth:1.0.0`)
업데이트 전에 만들어진 사본을 복원하세요.

```sh
docker compose stop berth
docker compose run --rm berth admin restore /data/backups/auto-<time>.sqlite
docker compose up -d
```

### 인터넷에서 접속하기

Berth는 일반 HTTP로 서비스해요. 집에서는 이것으로 충분하지만, 외부에서 접속하려면 앞단에 HTTPS를 지원하는
리버스 프록시를 두세요. 예를 들어 [Caddy](https://caddyserver.com)와 다음 `Caddyfile`을 쓸 수 있어요.

```
books.example.com {
	reverse_proxy localhost:8080
}
```

그런 다음 관리자의 설정 → 인터넷 주소에 `https://books.example.com`을 입력하면
휴대폰용 QR 코드에 이 주소가 포함돼요.

> [!WARNING]
> `./data`는 로컬 디스크에 두세요. 네트워크 공유(NFS, SMB)에 있는 SQLite는 데이터베이스를 손상시킬 수 있어요.

### 소스에서 빌드하기

이 리포지토리를 클론한 곳에서 `docker-compose.yml`의 `build: .` 주석을 해제하고
`docker compose up -d --build`를 실행하세요.

### 백업과 복원

- 관리자의 설정 → 백업에서 데이터베이스 사본을 다운로드하고, 자동 백업이나 업로드한 파일로
  복원할 수 있어요. 현재 데이터베이스는 먼저 자동 백업에 저장되므로, 같은 방법으로
  복원을 되돌릴 수 있어요.
- 콘솔에서(서버를 멈춘 상태로): `berth admin restore <file>`.

### 로그

로그에는 설정, 로그인(실패한 로그인은 주소와 함께), 휴대폰 페어링, 사용자, 접근 권한과
설정 변경, 업로드, 책에 관한 기록이 남아요. 비밀번호와 토큰은 절대 기록되지 않아요. 컨테이너 환경에
`RUST_LOG=info,tower_http=debug`를 설정하면 모든 HTTP 요청도 기록돼요.

## 리포지토리 구조

| 경로 | 설명 |
|---|---|
| [`crates/berth-cli`](crates/berth-cli) | `berth` — 서버 실행 파일 |
| [`crates/berth-server`](crates/berth-server) | HTTP 서버: axum 기반 JSON API v1과 웹 관리자 |
| [`crates/berth-core`](crates/berth-core) | 핵심 도메인 타입 |
| [`openapi.json`](openapi.json) | API 계약. API가 바뀌면 `cargo test`가 실패해요 |

## 기여하기

기여는 언제나 환영해요. 먼저 [CONTRIBUTING.md](CONTRIBUTING.md)를 읽어 주세요. 커밋에는 DCO에 따른 sign-off가 필요하고, Conventional Commits 규칙을 따라요.

## 보안

취약점은 공개 이슈로 알리지 말아 주세요. [SECURITY.md](SECURITY.md)를 참고하세요.

## 라이선스

[Apache License 2.0](LICENSE)
