# Bridge

Рабочее название. Объединяет устройства одной сети (ПК, ноутбук, телефон) в одно рабочее место:
общий курсор и клавиатура, перетаскивание файлов курсором между устройствами, общий буфер обмена,
проброс MIDI. Платформы: Windows 10/11, macOS 13+, Linux (X11, Wayland), Android 10+.

**Статус:** этап 1. Готовы:

- веха 0 — каркас и CI;
- веха 1 — протокол;
- веха 2 — ядро, QUIC-транспорт с закреплением сертификатов, демон `bridged`, локальный IPC и CLI `bridge`.

Два демона уже находят друг друга по адресу и держат зашифрованное соединение с переподключением.
Сопряжения, общего курсора и других функций для пользователя пока нет.

## Попробовать (два демона на одной машине)

```bash
bridged --name a --listen 127.0.0.1:47810 --data-dir ./a --ipc bridge-a
bridge --ipc bridge-a status
```

Временный способ доверия до вехи 3 (сопряжение): узнать id каждого устройства через `bridge status`,
затем перезапустить демоны с `--trust <id другого>`, а один из них — с `--connect <адрес>=<id>`.
План вех — [docs/ROADMAP.md](docs/ROADMAP.md).

## Документация

- [Архитектура и модель угроз](docs/ARCHITECTURE.md)
- [Архитектурные решения (ADR)](docs/adr/README.md)
- [Ограничения платформ](docs/LIMITATIONS.md)
- [Дорожная карта](docs/ROADMAP.md)

## Сборка

Нужен `rustup`; версия toolchain закреплена в `rust-toolchain.toml` и ставится автоматически.

```bash
cargo build --workspace
```

Проверки, которые выполняет CI:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo xtask check-deps
cargo deny check
```

## Лицензии зависимостей

Разрешены MIT, Apache-2.0, BSD, ISC, Zlib и Unicode-3.0. MPL-2.0 допускается только для перечисленных
в `deny.toml` немодифицированных сторонних crates. GPL, LGPL и AGPL запрещены
([ADR-0013](docs/adr/0013-licensing-and-dependencies.md)).
