# CS2 Autodirector Fix

![Platform](https://img.shields.io/badge/platform-Windows%20x64-0078d4?logo=windows&logoColor=white)
[![CS2](https://img.shields.io/badge/game-Counter--Strike%202-f3a712)](#english)
[![HLAE](https://img.shields.io/badge/HLAE-mirv__loadbinary-6f42c1)](https://github.com/advancedfx/advancedfx)
[![Release](https://img.shields.io/badge/release-manual-blue)](https://github.com/TsaplinIA/CS2-autodirector-fix/releases)
[![License](https://img.shields.io/badge/license-MIT-green)](LICENSE)
[![Status](https://img.shields.io/badge/status-experimental-orange)](#usage)

Language: [English](#english) | [Русский](#русский)

## English

### What This Fixes

`spec_autodirector true` in CS2 can cause weapon and hand jitter, unusual recoil
rendering, and small jumps in the first-person camera. This DLL bypasses the
broken autodirector-specific camera path for the camera types you choose.

Load it into CS2 through [HLAE](https://github.com/advancedfx/advancedfx), and
the first-person autodirector view should use the normal camera setup instead.

### Example

https://github.com/user-attachments/assets/fb6fc08f-caeb-4a60-a67f-e452e088758a

### Usage

1. Download the latest `autodirector-fix-vX.Y.Z.zip` from
   [Releases](https://github.com/TsaplinIA/CS2-autodirector-fix/releases).
2. Extract the archive. It contains:
   - `autodirector-fix-vX.Y.Z.dll`
   - `autodirector-fix-config.toml`
3. Start CS2 through [HLAE](https://github.com/advancedfx/advancedfx).
4. Load the DLL in the HLAE / CS2 console:

```text
mirv_loadlibrary "C:\path\to\autodirector-fix-vX.Y.Z.dll"
```

### Configuration

The configuration file is optional but recommended. Keep it next to the DLL.
If it is missing, these defaults are used:

```toml
[cameras]
fixed = true
first_person = false
chase = true
cameraman = true
```

`true` keeps the original CS2 autodirector behavior for that camera type.
`false` uses the normal camera setup instead.

### Logs

Each DLL injection creates a new `autodirector_fix-<timestamp>-<n>.log` file
next to the DLL.

### Contact

For support, contact [@Cool8ilya](https://t.me/Cool8ilya) on Telegram.

## Русский

### Что исправляет фикс

При `spec_autodirector true` в CS2 могут появляться подёргивания рук и оружия,
необычное отображение отдачи и небольшие скачки камеры от первого лица. Эта DLL
отключает проблемную ветку камеры autodirector для выбранных типов камер.

Загрузи DLL в CS2 через [HLAE](https://github.com/advancedfx/advancedfx), и
autodirector будет использовать обычную настройку камеры для этих режимов.

### Пример

https://github.com/user-attachments/assets/fb6fc08f-caeb-4a60-a67f-e452e088758a

### Использование

1. Скачай последний `autodirector-fix-vX.Y.Z.zip` из
   [Releases](https://github.com/TsaplinIA/CS2-autodirector-fix/releases).
2. Распакуй архив. В нём находятся:
   - `autodirector-fix-vX.Y.Z.dll`
   - `autodirector-fix-config.toml`
3. Запусти CS2 через [HLAE](https://github.com/advancedfx/advancedfx).
4. Загрузи DLL в консоли HLAE / CS2:

```text
mirv_loadlibrary "C:\path\to\autodirector-fix-vX.Y.Z.dll"
```

### Конфигурация

Конфиг необязателен, но его лучше оставить рядом с DLL. Если файла нет,
используются такие значения:

```toml
[cameras]
fixed = true
first_person = false
chase = true
cameraman = true
```

`true` оставляет исходное поведение autodirector для этого типа камеры.
`false` использует обычную настройку камеры.

### Логи

При каждой новой загрузке DLL рядом с ней создаётся новый файл
`autodirector_fix-<timestamp>-<n>.log`.

### Связь

По вопросам можно написать в Telegram: [@Cool8ilya](https://t.me/Cool8ilya).
