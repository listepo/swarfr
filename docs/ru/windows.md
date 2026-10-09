# Тесты на Windows

Набор тестов запускается в локальной виртуальной машине Windows. CI только проверяет, что
бэкенды Windows компилируются, и сами тесты не выполняет. Это тот же выбор, что и виртуальная
машина lima для бэкенда Linux.

## Гостевая система

На Apple Silicon гость — Windows 11 на ARM. Гипервизор —
[UTM](https://github.com/utmapp/UTM). Этот репозиторий его не ставит, и отсюда на машину, где
живёт гость, ничего не устанавливается. Гость, поднятый иначе, проходит те же шаги, как только
работает Windows 11 на ARM.

Диску виртуальной машины хватает на Windows и на второй динамически растущий VHDX не меньше
50 ГБ. Системный диск остаётся NTFS.

## Репозиторий

Внутри гостя клонировать по сети:

```bash
git clone https://github.com/listepo/swarfr
cd swarfr
```

Общая папка с Mac не заменяет клон. Тесты пишут фикстуры во временный каталог, и этот каталог
должен лежать на томе, который проверяется.

## Инструменты

Версия зафиксирована в `rust-toolchain.toml` (канал 1.98). Поставить mise так, как описано в
его инструкции для Windows, затем в клоне:

```bash
mise install
```

`just check` — это набор тестов. Запускать его из Git Bash, чтобы `sh` был в `PATH`. Рецепт,
который идёт после тестов, — сценарий оболочки, и он ничего не делает, если `swarfr` не
установлен.

## NTFS

Системный диск — том NTFS. Из клона, только на эту сессию:

```powershell
New-Item -ItemType Directory -Force -Path C:\swarfr-tmp | Out-Null
$env:TEMP = 'C:\swarfr-tmp'
$env:TMP = 'C:\swarfr-tmp'
just check
```

`TEMP` и `TMP` выбирают том, как `TMPDIR` выбирал btrfs или ext4 в виртуальной машине Linux.
Оставить их переменными сессии. Постоянный `setx` отправит следующий запуск на тот же том.

## ReFS

Dev Drive на ReFS, на VHDX. Windows 11 сборки 10.0.22621.2338 или новее, 50 ГБ свободно и
права администратора. Шаги, которые публикует Microsoft:
[Set up a Dev Drive on Windows 11](https://learn.microsoft.com/en-us/windows/dev-drive/).

Путь в параметрах, как в инструкции Microsoft: System, Storage, Advanced storage settings,
Disks & volumes, Create Dev Drive, затем **Create new VHD**. Выбрать VHDX, динамическое
расширение, не меньше 50 ГБ (64 ГБ достаточно). Мастер форматирует новый том как Dev Drive.
Форматировать его ещё раз не нужно.

Из командной строки, если VHDX не создаётся мастером, повышенный `diskpart` делает диск и
букву, а формат, который помечает Dev Drive, — отдельная команда. `maximum` задаётся в
мегабайтах:

```text
create vdisk file="C:\Users\%USERNAME%\swarfr-refs.vhdx" maximum=65536 type=expandable
select vdisk file="C:\Users\%USERNAME%\swarfr-refs.vhdx"
attach vdisk
convert gpt
create partition primary
assign letter=R
```

Затем, всё ещё с повышением, либо:

```text
Format R: /DevDrv /Q
```

либо в PowerShell:

```powershell
Format-Volume -DriveLetter R -DevDrive
```

Именно этот формат помечает том. Уже существующий том потом не превратить. В примерах буква
`R:`; подставить ту, которую получил раздел.

Затем из клона:

```powershell
New-Item -ItemType Directory -Force -Path R:\swarfr-tmp | Out-Null
$env:TEMP = 'R:\swarfr-tmp'
$env:TMP = 'R:\swarfr-tmp'
just check
```

## Что проверяет запуск

`caps` на Windows равен `NONE` до T21, а `clone_file` возвращает `ErrorKind::Unsupported`.
Контрактный тест `a_clone_holds_the_bytes_of_its_source_or_refuses_to_pretend` ждёт эту ошибку
и на NTFS, и на ReFS. `clone_file`, который копирует байты, сделает тест зелёным, сломав
контракт.

Оба результата записать в карточку T24 в `plan.md`. Задача закончена, когда туда записан
запуск на NTFS.

## Что вместо этого делает CI

`just check-cross` проверяет типы для `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` и
`aarch64-pc-windows-msvc`. Последняя цель — та, на которой работает гость. CI не запускает
набор тестов на Windows.
