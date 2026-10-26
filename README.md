# Bridge

Рабочее название. Объединяет устройства одной сети (ПК, ноутбук, телефон) в одно рабочее место:
общий курсор и клавиатура, перетаскивание файлов курсором между устройствами, общий буфер обмена,
проброс MIDI. Платформы: Windows 10/11, macOS 13+, Linux (X11, Wayland), Android 10+.

**Статус:** этап 1, веха 0 — каркас репозитория и CI. Функций для пользователя пока нет.
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
