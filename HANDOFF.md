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
| 4c | `3d51159`, `a214883` | macOS (CGEventTap + CGEventPost, онбординг Accessibility; только `clippy --target aarch64-apple-darwin`), Wayland (InputCapture + RemoteDesktop + EIS через `ashpd`/`reis`; на композиторе не проверено) |
| 5 | `6500bbd` | буфер обмена: протокол, фича с ленивой выдачей, Windows и X11 с нативными тестами; macOS и чистый Wayland — `Unsupported` |
| 6a | `21d30ed` | движок файлов (докачка, BLAKE3, конфликты, отмена), bulk-соединение с LEDBAT, `bridge send/transfers/cancel` |
| 6b | `9112a00` | перетаскивание: передача кнопки роутером, staging, Windows OLE, X11 XDND (нативный тест), mock-интеграция |

Задержка ввода через QUIC на loopback: p50 72 мкс, p99 119 мкс. Покрытие protocol + core — 92,9%.
Bulk с LEDBAT (горло 40 Мбит/с, очередь 1 МБ): p99 пинга 15,7 мс при базовых 13,1 мс; стандартный контроллер — 203 мс.

**macOS — в самом конце этапа** (решение владельца: нет техники Apple). Ввод 4c написан и собирается, всё
остальное по macOS (буфер, drag, MIDI, сеть, раскладка, сборка UI) — `TODO(macOS-final)`, список в
ROADMAP («Перенесено в конец этапа: macOS»). Таргеты `aarch64-apple-darwin` и `x86_64-unknown-linux-gnu`
из rustup удалены ради места на C: — вернуть `rustup target add …` на этапе macOS.

## Следующее: веха 7 (MIDI)

Содержание и «готово, когда» — в `docs/ROADMAP.md`, решения — в ADR-0009, ARCHITECTURE §4.7 и §8.4.

Уже сделано и **не закоммичено** (черновики в рабочем дереве, в `lib.rs` не подключены, тесты не запускались):

- `crates/protocol/src/midi.rs` — `Ports`, `Events { port, events: [{ts, data}] }`, `is_valid_message`,
  `data_len`. Подключить: `pub mod midi;` в `crates/protocol/src/lib.rs`.
- `crates/core/src/midi.rs` — `MidiParser` (running status, real-time, SysEx ≤ 64 КиБ), `NoteTracker`
  (ноты + педали CC64/66/67 → `release_all`), `JitterBuffer` (`ts_local + min_transit + buffer`, play-время
  не убывает, опоздавшие — сразу, адаптивный режим по p99). Подключить: `pub mod midi;` в `core/src/lib.rs`.

План (принятые решения):

1. В `PeerView` добавить `clock_offset: Option<i64>` из `Core::clock()` (это `their − our`, мкс), чтобы
   переводить метки отправителя в своё время.
2. Трейт `MidiPorts` в `platform`: `inputs/outputs`, `open_input(name, sink)`, `open_output(name)`,
   `virtual_source(name)` (приложения видят как вход), `virtual_destination(name, sink)`, `is_own(name)`.
   Метка времени в `MidiSink::on_midi` — `Instant`, фича переводит её в `Micros` узла через якорь
   `(cx.now(), Instant::now())`.
3. Фича `bridge-feature-midi`:
   - канал `ChannelClass::Midi`, право `MIDI`;
   - отправитель шлёт `Ports` и пачки `Events` (кадр ≤ 72 КиБ);
   - у приёмника — **свой поток проигрывания** с точным сном. Тик узла слишком грубый для буфера в 3 мс;
   - `NoteTracker` на (пир, порт): при разрыве — note-off и педали в 0, порт живёт 10 с, при `Left` закрывается.
4. Платформы:
   - **Windows** — `midir 0.11` (WinMM, тянет `windows` 0.59–0.62, совпадёт с нашим 0.62) для физических портов.
     Виртуальные порты — режим loopMIDI: пользователь создаёт `Bridge In`/`Bridge Out`, мы их находим по
     имени. Одно соединение на порт держать в кэше.
   - **Spike WMS:** служба `MidiSrv` на этой машине есть (Windows 11 25H2), но привязки
     `Windows.Devices.Midi2` требуют `.winmd` из NuGet SDK. Если без сети не получить — `TODO(M7+)` и
     LIMITATIONS.
   - **Linux** — ALSA sequencer через `dlopen("libasound.so.2")` (`libloading` 0.7.4 в кэше, ISC), без
     `alsa-sys`: в WSL нет ни `libasound2-dev`, ни `pkg-config`, ни `/dev/snd`. Минимальный FFI:
     `snd_seq_open/create_simple_port/event_output_direct/event_input/connect_from/connect_to`,
     `snd_midi_event_*`; `snd_seq_event_t` — `#[repr(C)]`, 28 байт. Нет библиотеки — `Unsupported`.
   - **macOS** — в конце этапа.
5. Тесты:
   - proptest монотонности jitter-буфера;
   - netsim с потерями и джиттером: порядок и полнота;
   - отпускание нот при разрыве;
   - бенчмарк задержки и джиттера (mock-порты, цель из ADR — < 5 мс поверх сети) + запись в `benches/targets.toml`.
6. MANUAL_TESTS: DAW (Ableton/Reaper/FL/Bitwig), loopMIDI, «выдернуть сеть с зажатой нотой».

## Дальше: вехи 8–11

- 8 — состояния Pause/Leave/Rejoin, `NetworkMonitor`, профили сетей (ADR-0016);
- 9 — Tauri + Svelte UI (ADR-0010). Нужен Node (есть, v24). Сюда же:
  - оверлеи (край, локатор), диалог подтверждения входящих файлов, режим хоткея для Wayland без InputCapture;
- 10 — Android (ADR-0012). Android SDK/JDK на машине нет;
- 11 — выпуск этапа, затем блок macOS.

Важные TODO от прошлых вех:

- виртуальные файлы Windows (`FILEDESCRIPTORW`/`FILECONTENTS`) для drag и формата FILES в буфере;
- буфер на чистом Wayland (`ext-data-control`);
- раскладка на X11/Wayland/macOS;
- персистентные передачи файлов.

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
  - X11 в WSLg: `DISPLAY=:0 BRIDGE_X11_TESTS=1 cargo test -p bridge-platform --features mock --test x11_native -- --test-threads=1`
    (тесты двигают один курсор — только последовательно);
  - mDNS: `BRIDGE_MDNS_TEST=1`.
- **Демоны с вводом** на этой машине не запускать парами: два набора хуков на одном рабочем столе мешают
  пользователю. Дымовые тесты двух `bridged` — только с `--no-input`.
- **Права в WSL**: `sudo` с паролем, поэтому пакеты (xvfb, libasound2-dev) ставит владелец.
- **Диск C: был заполнен до 0 байт.** Виртуальный диск WSL (`ext4.vhdx`, 20 ГБ) не смог расти, и git в WSL
  падал с `Input/output error`. Помогли `wsl --shutdown` и чистка `~/bridge-target/*/incremental`. В WSL
  собирать с `CARGO_INCREMENTAL=0`. Если git в WSL снова даёт I/O error — проверить свободное место на C:.
- **Нативные Windows-тесты** трогают живой рабочий стол. Пока владелец работает за машиной (или включён экран
  блокировки), `SendInput` и захват буфера иногда отказывают. По отдельности тесты проходят.
- **Сетевой симулятор на Windows**: тесты с bulk-трафиком вызывают `bridge_testkit::fine_timers()`
  (`timeBeginPeriod(1)`). Иначе шаг таймера 15,6 мс выглядит для LEDBAT как очередь.
- **Полный прогон перед коммитом**:
  1. `cargo fmt --all`;
  2. `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
  3. `cargo test --workspace --locked`;
  4. `cargo xtask check-deps`;
  5. `cargo deny ... licenses bans sources`;
  6. `cargo llvm-cov -p bridge-protocol -p bridge-core --ignore-filename-regex 'strategy\.rs' --fail-under-lines 85`;
  7. нативные тесты (Windows, X11 в WSLg) — для вех, где менялась платформа;
  8. то же самое в WSL.
- **Раскладка экранов в тестах.** Чей экран окажется справа, зависит от того, чья версия раскладки победит.
  Тесты определяют направление по раскладке (`testkit/tests/input.rs::direction`).
