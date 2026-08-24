# CS2 Autodirector Fix

[![Rust](https://img.shields.io/badge/Rust-stable-b7410e?logo=rust&logoColor=white)](https://www.rust-lang.org/)
![Platform](https://img.shields.io/badge/platform-Windows%20x64-0078d4?logo=windows&logoColor=white)
[![CS2](https://img.shields.io/badge/game-Counter--Strike%202-f3a712)](#english)
[![HLAE](https://img.shields.io/badge/HLAE-mirv__loadbinary-6f42c1)](https://github.com/advancedfx/advancedfx)
[![Release](https://img.shields.io/badge/release-manual-blue)](https://github.com/TsaplinIA/CS2-autodirector-fix/releases)
[![License](https://img.shields.io/badge/license-MIT-green)](LICENSE)
[![Status](https://img.shields.io/badge/status-experimental-orange)](#usage)

Language: [English](#english) | [Русский](#русский)

## English

### What This Fixes

Anyone who tried to use `spec_autodirector true` in CS2 has probably run into a
lot of visual bugs. The most annoying ones are weapon and hand jitter, strange
recoil rendering, small view jumps, and similar first-person camera artifacts.

This gets in the way of using `spec_autodirector true` for commentary on small
events. Autodirector is useful when there is no dedicated observer, but the
first-person bugs make the picture look broken.

This project is a small fix for those bugs. It is distributed as a DLL: load it
into CS2, for example through [HLAE](https://github.com/advancedfx/advancedfx)
with `mirv_loadlibrary`, and the weapon jitter / view jumps on the autodirector
first-person path should disappear.

Star us on GitHub - your support motivates us a lot! It also helps other
people find this fix faster.

### Examples

Synchronized before/after GIF or WebP previews will be added later.

https://github.com/user-attachments/assets/fb6fc08f-caeb-4a60-a67f-e452e088758a

### Usage

1. Download the latest `autodirector-fix-vX.Y.Z.zip` from
   [Releases](https://github.com/TsaplinIA/CS2-autodirector-fix/releases).
2. Extract the archive. It contains:
   - `autodirector-fix-vX.Y.Z.dll`
   - `autodirector-fix-config.toml`
3. Start CS2 through [HLAE](https://github.com/advancedfx/advancedfx).
4. In the HLAE / CS2 console, load the DLL:

```text
mirv_loadlibrary "C:\path\to\autodirector-fix-vX.Y.Z.dll"
```

Usage preview WebP will be added later.

The config file is optional but recommended. It must be placed next to the DLL.
If the file is missing, the DLL uses the same defaults:

```toml
[cameras]
fixed = true
first_person = false
chase = true
cameraman = true
```

`true` keeps the original CS2 autodirector override for that camera type.
`false` bypasses that override and falls back to the normal camera setup.

The DLL writes logs to the CS2 console with the `[autodirector-fix]` prefix and
also creates `autodirector_fix.log` next to the DLL.

### Build Locally

Requirements:

- Windows x64
- Rust stable
- MSVC Rust target: `x86_64-pc-windows-msvc`

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --release
cargo test
```

Local DLL output:

```text
target\release\autodirector_fix.dll
```

For local manual testing, keep `autodirector-fix-config.toml` next to the DLL
you load with `mirv_loadlibrary`.

### Contacts

There is no Discord or dedicated support channel yet. This may appear later.
For now, you can contact the author on Telegram: [@Cool8ilya](https://t.me/Cool8ilya).

## Русский

### Что Исправляет

Любой, кто пытался использовать `spec_autodirector true` в CS2, скорее всего
сталкивался с большим количеством визуальных багов. Самые бесячие:
подергивание рук и оружия, странное отображение отдачи, небольшие прыжки камеры
и похожие артефакты камеры от первого лица.

Это мешает использовать `spec_autodirector true` комментаторам на маленьких
ивентах. Автодиректор полезен, когда нет отдельного обсервера, но из-за багов
камеры от первого лица картинка выглядит сломанной.

Этот проект - небольшой фикс для этих багов. Он поставляется как DLL: загрузи
ее в CS2, например через [HLAE](https://github.com/advancedfx/advancedfx)
командой `mirv_loadlibrary`, и подергивания оружия / прыжки камеры на пути
автодиректора от первого лица должны пропасть.

Поставь звезду на GitHub - твоя поддержка очень мотивирует! Так другим людям
будет проще найти этот фикс.

### Примеры

https://github.com/user-attachments/assets/fb6fc08f-caeb-4a60-a67f-e452e088758a

### Использование

1. Скачай последний `autodirector-fix-vX.Y.Z.zip` в
   [разделе релизов](https://github.com/TsaplinIA/CS2-autodirector-fix/releases).
2. Распакуй архив. Внутри лежат:
   - `autodirector-fix-vX.Y.Z.dll`
   - `autodirector-fix-config.toml`
3. Запусти CS2 через [HLAE](https://github.com/advancedfx/advancedfx).
4. В консоли HLAE / CS2 загрузи DLL:

```text
mirv_loadlibrary "C:\path\to\autodirector-fix-vX.Y.Z.dll"
```

WebP-пример загрузки через HLAE будет добавлен позже.

Конфиг опциональный, но его лучше оставить рядом с DLL. Если файла нет, DLL
использует такие же значения по умолчанию:

```toml
[cameras]
fixed = true
first_person = false
chase = true
cameraman = true
```

`true` оставляет оригинальный оверрайд автодиректора CS2 для этого типа
камеры. `false` отключает оверрайд и возвращает камеру в обычную логику
настройки вида.

DLL пишет логи в консоль CS2 с префиксом `[autodirector-fix]`, а также создает
`autodirector_fix.log` рядом с DLL.

### Сборка У Себя

Требования:

- Windows x64
- стабильный Rust
- MSVC-цель Rust: `x86_64-pc-windows-msvc`

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --release
cargo test
```

Локальная DLL появится здесь:

```text
target\release\autodirector_fix.dll
```

Для ручного локального теста положи `autodirector-fix-config.toml` рядом с DLL,
которую загружаешь через `mirv_loadlibrary`.

### Контакты

Дискорда или отдельного канала поддержки пока нет. Возможно, они появятся
позже. Сейчас можно писать автору в Telegram:
[@Cool8ilya](https://t.me/Cool8ilya).
