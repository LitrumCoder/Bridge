# Bridge — передача работы между сессиями

Читать первым в новой сессии. Исходное ТЗ — в первом сообщении владельца. Его суть закреплена в
`docs/ARCHITECTURE.md`, `docs/adr/`, `docs/ROADMAP.md`. Состояние на 2026-10-27.

## Договорённости с владельцем

- План вех из `docs/ROADMAP.md` утверждён. Каждая веха — это код, тесты, зелёные проверки и коммит.
- Лицензии (ADR-0013):
  - разрешены MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, 0BSD, CC0;
  - MPL-2.0 — только для перечисленных в `deny.toml` немодифицированных crates;
  - системные библиотеки ОС подключаются динамически (glibc, libasound, WebKitGTK);
  - GPL, LGPL, AGPL запрещены.
- Android-мост — UniFFI.
- Владелец просил идти по вехам **без вопросов**. Решения принимать самому, всё записывать в ADR и LIMITATIONS.
- Никаких заглушек под видом готового: только явный `TODO(Mx)` и пункт в ROADMAP. Упёрлись в ограничение
  ОС — описать в `docs/LIMITATIONS.md`.
- Документация пишется по-русски, код и комментарии — по-английски, комментариев мало.
- Рутинное чтение документации и исходников отдавать сабагентам (`model: haiku`).
- API библиотек сверять по исходникам в
  `C:\Users\user\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\<crate>-<ver>\`, а не по памяти.

## Готово (коммиты в `main`, только локально)

| Веха | Коммит | Что |
|---|---|---|
| 0 | `0212083` | workspace, `deny.toml`, xtask (`check-deps`, `bench-check`), CI |
| 1 | `8f5082c` | протокол: конверт, кадры, Bounded-типы, control-сообщения, версии, golden, proptest, fuzz (`fuzz/`) |
| 2 | `29f3a1d` | sans-IO `Core`, Feature/Registry, QUIC (quinn + rustls/ring), netsim, `bridge_node`, IPC, `bridged`, `bridge` |
| 3 | `9e6834a` | mDNS (`mdns-sd`), SPAKE2-сопряжение по коду и QR, `trust.toml`, права, отзыв, лимиты рукопожатий |
| 4a | `b4330e2` | геометрия, InputRouter, KeyTracker, хоткеи, сторож, синхронизация раскладки, фича `input`, mock-платформа |
| 4b | `dbc4455` | Windows (LL-хуки + SendInput) и X11 (XInput2 + XTest + RandR), ввод включён в `bridged` |
| 4c (часть) | `3d51159` | таблица HID ↔ macOS `kVK_*` в `crates/platform/src/keymap.rs` |

Задержка ввода через QUIC на loopback: p50 72 мкс, p99 119 мкс. Покрытие protocol + core — 92%.

## Следующее: веха 4c

1. **macOS** (`crates/platform/src/macos/`, `#[cfg(target_os = "macos")]`):
   - зависимости: `objc2-core-graphics 0.3.2`, `objc2-core-foundation 0.3.2`, `objc2-application-services 0.3.2`
     (MIT/Zlib/Apache). Сначала выписать точные сигнатуры и feature-флаги из исходников (сабагентом);
   - захват — сессионный `CGEventTap` (`Default`, с подавлением) на своём потоке с `CFRunLoop`. Нажатия — по
     `kCGKeyboardEventKeycode` → `mac_to_hid`; дельты — `kCGMouseEventDeltaX/Y`. В режиме захвата —
     `CGAssociateMouseAndMouseCursorPosition(false)` и подавление (вернуть NULL). Свою инъекцию отличать по
     `kCGEventSourceUserData` = метка. При `TapDisabledByTimeout` включать tap заново, при `ByUserInput` — снимать;
   - инъекция — `CGEventCreateKeyboardEvent`, `CGEventCreateMouseEvent`, скролл, затем `CGEventPost`;
   - экраны — `CGGetActiveDisplayList` + `CGDisplayBounds` + `CGDisplayScreenSize` (мм);
   - онбординг: `AXIsProcessTrustedWithOptions(prompt)` и понятная ошибка `PermissionRequired`;
   - проверка: `rustup target add aarch64-apple-darwin`, затем
     `cargo check --target aarch64-apple-darwin -p bridge-platform` (линковка не нужна). Запустить нельзя —
     пометить в LIMITATIONS и MANUAL_TESTS.
2. **Wayland**:
   - `native()` в `crates/platform/src/lib.rs` должна при `XDG_SESSION_TYPE=wayland` не брать X11-бэкенд:
     через XWayland он видит только X-клиентов;
   - бэкенд на `ashpd 0.13` (порталы InputCapture и RemoteDesktop, `connect_to_eis`) и `reis 0.7` (MIT);
   - при отсутствии порталов — `PlatformError::Unsupported` с текстом для пользователя.
     Режим «переключение по хоткею» — через портал GlobalShortcuts или только для окна в фокусе (ADR-0015);
   - проверить здесь негде: пометить как непроверенное.
3. Добавить в CI задачу `macos-latest`: сборка и юнит-тесты (в матрице уже есть).

## Дальше: вехи 5–11

Их содержание — в `docs/ROADMAP.md`, решения — в ADR:

- 0007 — буфер обмена;
- 0008 — перетаскивание;
- 0009 — MIDI;
- 0010 — UI;
- 0012 — Android;
- 0014 — тесты.

Важно:

- **Веха 6a.** Bulk-соединение с delay-based контроллером: свой `quinn::congestion::Controller`. Для него нужна
  прямая зависимость `quinn-proto 0.11.19` (`RttEstimator` не реэкспортирован). Тест — p99 задержки ввода при
  передаче 2 ГБ через netsim с узким горлом.
- **Веха 7.** На Windows сначала проверить Windows MIDI Services (только 11 25H2+). Запасной путь — loopMIDI,
  который ставит пользователь.
- **Вехи 9 и 10.** Нужны Node (есть, v24) и Android SDK/JDK (нет на машине).

## Окружение и подводные камни

- **git** есть только в WSL: `wsl -e bash -c "cd '/mnt/d/клодик/bridge' && git ..."`. Коммит — через скрипт-файл
  с сообщением из файла (`git commit -F`).
- **Remote и push** блокирует классификатор автодействий («Remote Repoint»). Делает владелец после создания
  репозитория (поле `repository` в `Cargo.toml` — заглушка `OWNER/bridge`).
- **Rust** 1.99.0 закреплён в `rust-toolchain.toml`. В WSL: `source ~/.cargo/env; export CARGO_TARGET_DIR=~/bridge-target`.
  Nightly и cargo-fuzz в WSL установлены.
- **PowerShell 5.1**:
  - не переписывать UTF-8 файлы через `Get-Content`/`Set-Content` — правки через Edit/Write;
  - `py -3 -c` с кавычками ломается — писать .py/.sh/.ps1 в scratchpad;
  - .ps1 с кириллическими путями сохранять с BOM или передавать путь параметром.
- **Python** — `py -3`; `python3` — заглушка из Store.
- **cargo-deny** на Windows не скачивает базу advisories (нет git). Локально проверять
  `cargo deny check licenses bans sources`, advisories — в CI.
- **Платформенные тесты** (опциональные):
  - Windows: `BRIDGE_PLATFORM_TESTS=1 cargo test -p bridge-platform --features mock --test windows_native -- --test-threads=1`;
  - X11 в WSLg: `DISPLAY=:0 BRIDGE_X11_TESTS=1 cargo test -p bridge-platform --features mock --test x11_native`;
  - mDNS: `BRIDGE_MDNS_TEST=1`.
- **Демоны с вводом** на этой машине не запускать парами: два набора хуков на одном рабочем столе мешают
  пользователю. Дымовые тесты двух `bridged` — только с `--no-input`.
- **Права в WSL**: `sudo` с паролем, поэтому пакеты (xvfb, libasound2-dev) ставит владелец.
- **Полный прогон перед коммитом**:
  1. `cargo fmt --all`;
  2. `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
  3. `cargo test --workspace --locked`;
  4. `cargo xtask check-deps`;
  5. `cargo deny ... licenses bans sources`;
  6. `cargo llvm-cov -p bridge-protocol -p bridge-core --ignore-filename-regex 'strategy\.rs' --fail-under-lines 85`;
  7. то же самое в WSL.
- **Раскладка экранов в тестах.** Чей экран окажется справа, зависит от того, чья версия раскладки победит.
  Тесты определяют направление по раскладке (`testkit/tests/input.rs::direction`).
