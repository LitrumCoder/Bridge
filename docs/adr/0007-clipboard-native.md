# ADR-0007. Буфер обмена: нативные реализации с отложенной выдачей

Статус: Принято (веха 5: Windows, X11; macOS и Wayland — позже)

## Контекст

Нужны текст, HTML, RTF, изображения и файлы; файлы — лениво, только при вставке. Ни один кроссплатформенный
crate не даёт отложенную выдачу: `arboard 3.7` — текст, изображения, `set_html`, без списков файлов;
`clipboard-rs 0.3` — файлы и HTML, но данные надо положить сразу. Также нужно уважать пометки менеджеров
паролей и не создавать эхо между устройствами.

## Решение

Собственные реализации трейта `Clipboard` в `platform`:

- **Windows:** окно-слушатель `AddClipboardFormatListener`; объявление форматов `SetClipboardData(fmt, NULL)`
  и выдача по `WM_RENDERFORMAT`/`WM_RENDERALLFORMATS`; файлы — `CFSTR_FILEDESCRIPTORW`/`CFSTR_FILECONTENTS`
  (виртуальные файлы, Проводник показывает свой прогресс копирования); пропуск содержимого с форматом
  `ExcludeClipboardContentFromMonitorProcessing`.
- **macOS:** опрос `NSPasteboard.changeCount` (событий нет) раз в 250 мс; выдача через
  `NSPasteboardItemDataProvider`; файлы — file promises; пропуск `org.nspasteboard.ConcealedType`.
- **X11:** XFixes `SelectionNotify` для отслеживания, становимся владельцем `CLIPBOARD` — выдача по запросу
  естественна для X11; INCR для больших данных; пропуск `x-kde-passwordManagerHint`.
- **Wayland:** `ext-data-control-v1`/`zwlr-data-control-v1`, если композитор даёт; иначе синхронизация только
  когда окно Bridge в фокусе (LIMITATIONS).
- **Android:** чтение при открытии приложения, по кнопке и из плитки быстрых настроек (ADR-0012).

Протокол: `Offer` с форматами и размерами; маленький текст (≤ 256 КиБ) отправляется сразу; изображения
передаются как PNG с лимитом на размер в пикселях до декодирования; файлы — через движок `files`
при запросе выдачи.

## Реализация (веха 5)

- Протокол (`bridge_protocol::clipboard`): `Offer { seq, formats: [{format, size, inline?}] }`, `Request`,
  `Data { offset, total, bytes ≤ 512 КиБ }`, `Unavailable`. `ClipFormat` — открытый код: TEXT (UTF-8, `\n`),
  HTML (UTF-8), RTF, PNG, FILES. Неизвестные форматы пропускаются. Инлайн — до 256 КиБ на всё предложение,
  один формат — до 256 МиБ на проводе. Приёмник по умолчанию принимает до 64 МиБ. Содержимое не попадает
  в `Debug` (`BoundedBytes`).
- Фича (`bridge-feature-clipboard`):
  - текстовые форматы читаются сразу и уходят инлайн, если помещаются;
  - PNG и длинные тексты объявляются и запрашиваются только при вставке. Вставка блокирует поток буфера ОС,
    а не поток узла; таймаут — 15 с;
  - эхо отсекается флагом `own` от платформы;
  - устаревший `seq` получает `Unavailable`;
  - предложения уходят только пирам в состоянии `Connected`, которые разрешили нам `CLIPBOARD`.
- Трейт `Clipboard` в `platform`: `start(sink)`, `read(format)`, `announce(formats, provider)`, `stop`.
  Конвертации (CRLF, `HTML Format`, PNG ↔ DIB с лимитом 64 Мпикс до декодирования) вынесены в чистый модуль
  `clipfmt`, он тестируется на всех ОС.
- **Windows.** Окно `HWND_MESSAGE` с `AddClipboardFormatListener`, отложенная выдача через
  `WM_RENDERFORMAT`. PNG объявляется как `PNG` и `CF_DIB`.
  - Наше объявление помечено `CanIncludeInClipboardHistory=0`, `CanUploadToCloudClipboard=0` и
    `ExcludeClipboardContentFromMonitorProcessing`: иначе журнал буфера (Win+V) и мониторы сразу вытягивают
    все форматы по сети. Проверка на этой машине показала, что сторонние мониторы всё равно читают текст.
  - При выходе (`WM_RENDERALLFORMATS`) чужое содержимое удаляется, а не скачивается целиком.
- **X11.** Владение `CLIPBOARD`, XFixes `SelectionNotify`, TARGETS, INCR в обе стороны. HTML в UTF-16 с BOM
  (Firefox) декодируется. `x-kde-passwordManagerHint` → `concealed`.
- **Wayland** без XWayland и **macOS** — `Unsupported` (`TODO`). На Wayland с XWayland работает X11-путь, но
  только пока в фокусе X-клиент. macOS владелец перенёс в конец этапа (ROADMAP).
- FILES ждёт движок файлов (веха 6a).

## Альтернативы

`arboard`/`clipboard-rs` с немедленной передачей всего содержимого — нарушает «файлы лениво», гоняет
мегабайты при каждом копировании.

## Последствия

Больше платформенного кода; mock-реализация обязательна для интеграционных тестов.
