# ADR-0013. Лицензионная политика и зависимости

Статус: Принято 2026-10-26. Владелец утвердил рекомендацию: MPL-2.0 — только для немодифицированных
сторонних crates (точечные исключения в `deny.toml`), Unicode-3.0 — глобально, системные библиотеки ОС с
динамической связью — списком ниже.

## Системные библиотеки ОС (динамическая связь, не поставляются)

| Библиотека | Лицензия | Зачем |
|---|---|---|
| glibc | LGPL-2.1+ | любой бинарник на Linux |
| libasound (alsa-lib) | LGPL-2.1+ | MIDI на Linux (ADR-0009) |
| WebKitGTK (через Tauri на Linux) | LGPL-2.0+/BSD | системный WebView десктоп-UI |

## Контекст

Продукт может стать коммерческим. Разрешены MIT / Apache-2.0 / BSD / ISC / Zlib, запрещены GPL / LGPL / AGPL.
Контроль — `cargo-deny` в CI (лицензии, RustSec, источники, дубликаты), для npm — `license-checker` по `package.json`.

Проверка версий и лицензий (2026-10-26, crates.io API / npm registry) выявила зависимости вне списка.

## Исключения, требующие решения

| Что | Лицензия | Где | Предложение |
|---|---|---|---|
| `uniffi` | MPL-2.0 | Android-мост (стек из ТЗ) | Разрешить MPL-2.0 для **немодифицированных** сторонних crates (см. ниже) или заменить ручным JNI (ADR-0012) |
| `option-ext` (через `directories`/`dirs`), `cssparser`/`selectors` (по предварительной оценке — транзитивно через Tauri) | MPL-2.0 | конфиг-пути, Tauri | То же правило; точный список даст первый прогон `cargo deny` на вехе 0 |
| `libasound` (alsa-lib, C) | LGPL-2.1+ | MIDI на Linux, динамически | Разрешить **системные библиотеки ОС, связываемые динамически и не поставляемые с продуктом** (тот же класс, что glibc — сама LGPL-2.1, без неё на Linux не работает ничего). `cargo-deny` их не видит; список ведётся в этом ADR. |
| `unicode-ident` (через `proc-macro2`, есть почти в любом Rust-проекте) | (MIT OR Apache-2.0) AND Unicode-3.0 | везде | Разрешить Unicode-3.0 — пермиссивная лицензия на данные Unicode |
| `libfuzzer-sys` | (MIT OR Apache-2.0) AND NCSA | только `fuzz/` | `fuzz/` — отдельный workspace, в продукт не входит; проверяется отдельным `deny.toml` с NCSA |

**MPL-2.0 по существу:** копилефт на уровне файлов. При использовании без изменений обязанность — сделать
доступными исходники самих MPL-файлов (они публичны на crates.io); собственный код и закрытые модули
не затрагиваются, коммерческое распространение разрешено. Это принципиально отличается от GPL/LGPL.

**Рекомендация:** разрешить `MPL-2.0` только для немодифицированных сторонних crates (явный список в
`deny.toml` через `exceptions`, не глобально), `Unicode-3.0` глобально, системные библиотеки ОС — списком
в этом ADR. GPL/LGPL/AGPL в crates — запрет без исключений.

Если строгий вариант (без MPL вообще): придётся заменить `directories` своим кодом (несложно), UniFFI —
ручным JNI (реально), но Tauri, вероятно, не пройдёт проверку — это противоречит стеку ТЗ и потребует
отдельного ADR о замене UI-оболочки.

## Решения, не требующие исключений

- rustls с провайдером **`ring`** (`Apache-2.0 AND ISC`), а не `aws-lc-rs` по умолчанию (в выражении
  `aws-lc-sys` есть MIT-0; плюс cmake/nasm в сборке) — ADR-0002.
- **postcard**, не bincode (RUSTSEC-2025-0141, проект прекращён) — ADR-0003.
- teVirtualMIDI и loopMIDI не распространяются (проприетарные) — ADR-0009.
- Код Deskflow/Input Leap/Barrier (GPL-2.0) не изучается. Mouse Without Borders (MIT) — только как
  справочник по поведению, код не копируется.

## Проверенные версии (2026-10-26)

| Crate / пакет | Версия | Лицензия | Примечание |
|---|---|---|---|
| tokio / tokio-util / bytes / futures | 1.54.1 / 0.7.20 / 1.12.1 / 0.3.34 | MIT (futures — MIT OR Apache) | |
| quinn / quinn-proto | 0.11.12 / 0.11.19 | MIT OR Apache-2.0 | datagrams, `set_priority(i32)` |
| rustls / rustls-pki-types | 0.23.46 / 1.15.1 | Apache/ISC/MIT | провайдер `ring` |
| ring | 0.17.14 | Apache-2.0 AND ISC | последний релиз 2025-03 |
| rcgen | 0.14.10 | MIT OR Apache-2.0 | P-256 |
| spake2 | 0.4.0 | MIT OR Apache-2.0 | |
| mdns-sd | 0.21.6 | Apache-2.0 OR MIT | |
| postcard / serde | 1.1.3 / 1.0.229 | MIT OR Apache-2.0 | |
| blake3 | 1.8.7 | CC0-1.0 OR Apache-2.0 OR … | берём Apache-2.0 |
| midir | 0.11.1 | MIT | виртуальные порты кроме Windows |
| alsa (Rust) | 0.12.1 | Apache-2.0/MIT | → libasound (см. исключения) |
| coremidi | 0.9.2 | MIT | альтернатива — objc2-core-midi |
| windows / windows-sys | 0.62.2 / 0.61.2 | MIT OR Apache-2.0 | |
| objc2 / objc2-app-kit / objc2-core-graphics / objc2-foundation | 0.6.5 / 0.3.2 / 0.3.2 / 0.3.2 | MIT / Zlib OR Apache OR MIT | |
| x11rb | 0.14.0 | MIT OR Apache-2.0 | модули xinput, xtest, xfixes |
| ashpd | 0.13.13 | MIT | порталы InputCapture, RemoteDesktop |
| reis | 0.7.1 | MIT | ранняя стадия (ADR-0015) |
| interprocess | 2.4.7 | 0BSD OR Apache-2.0 | |
| directories | 6.0.0 | MIT OR Apache-2.0 | транзитивно option-ext (MPL-2.0) |
| tracing / tracing-subscriber | 0.1.44 / 0.3.23 | MIT | |
| thiserror / anyhow / clap | 2.0.21 / 1.0.104 / 4.6.7 | MIT OR Apache-2.0 | |
| qrcode | 0.14.1 | MIT OR Apache-2.0 | |
| if-watch | 3.2.2 | MIT OR Apache-2.0 | |
| proptest / criterion | 1.12.0 / 0.8.2 | MIT OR Apache-2.0 | |
| cargo-deny / cargo-fuzz / cargo-llvm-cov | 0.20.2 / 0.13.2 / 0.9.1 | MIT OR Apache-2.0 | инструменты |
| uniffi | 0.32.2 | **MPL-2.0** | см. исключения |
| tauri / tauri-build | 2.12.5 / 2.7.4 | Apache-2.0 OR MIT | трей за feature `tray-icon` |
| tauri-plugin-single-instance / autostart / notification / dialog | 2.5.4 / 2.7.0 / 2.7.0 / 2.8.3 | Apache-2.0 OR MIT | стабильная ветка 2.x |
| svelte / vite / @sveltejs/vite-plugin-svelte | 5.57.2 / 8.3.5 / 7.3.1 | MIT | |
| vitest / @testing-library/svelte / @playwright/test | 5.0.5 / 5.4.2 / 1.64.1 | MIT / MIT / Apache-2.0 | |
| @tauri-apps/api / @tauri-apps/cli | 2.12.4 | Apache-2.0 OR MIT | |

Перед подключением каждой библиотеки на своей вехе API сверяется с docs.rs той же версии.

## Последствия

- `deny.toml`: `allow = [MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib,
  Unicode-3.0, 0BSD, CC0-1.0]` + точечные исключения по решению владельца; `[advisories]` с отказом
  на уязвимости и незаменённые unmaintained.
- `THIRD_PARTY_NOTICES` генерируется `cargo-about` (MIT/Apache) при сборке релиза.
