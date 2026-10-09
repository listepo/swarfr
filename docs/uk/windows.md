# Тести на Windows

Набір тестів запускається в локальній віртуальній машині Windows. CI лише перевіряє, що
бекенди Windows компілюються, і самі тести не виконує. Це той самий вибір, що й віртуальна
машина lima для бекенду Linux.

## Гостьова система

На Apple Silicon гість — Windows 11 на ARM. Гіпервізор —
[UTM](https://github.com/utmapp/UTM). Цей репозиторій його не встановлює, і звідси на машину,
де живе гість, нічого не ставиться. Гість, піднятий інакше, проходить ті самі кроки, щойно
працює Windows 11 на ARM.

Диска віртуальної машини вистачає на Windows і на другий VHDX, що динамічно зростає, не менше
50 ГБ. Системний диск лишається NTFS.

## Репозиторій

Усередині гостя клонувати через мережу:

```bash
git clone https://github.com/listepo/swarfr
cd swarfr
```

Спільна тека з Mac не замінює клон. Тести пишуть фікстури в тимчасовий каталог, і цей каталог
має лежати на томі, який перевіряється.

## Інструменти

Версію зафіксовано в `rust-toolchain.toml` (канал 1.98). Поставити mise так, як описано в його
інструкції для Windows, потім у клоні:

```bash
mise install
```

`just check` — це набір тестів. Рецепт, який іде після тестів, — сценарій оболонки, і він
нічого не робить, якщо `swarfr` не встановлено. Йому потрібен `sh` у `PATH`. Його дає Git for
Windows. З PowerShell, на цю сесію:

```powershell
$env:PATH = "C:\Program Files\Git\bin;$env:PATH"
```

## NTFS

Системний диск — том NTFS. З клона, лише на цю сесію:

```powershell
New-Item -ItemType Directory -Force -Path C:\swarfr-tmp | Out-Null
$env:TEMP = 'C:\swarfr-tmp'
$env:TMP = 'C:\swarfr-tmp'
just check
```

`TEMP` і `TMP` обирають том, як `TMPDIR` обирав btrfs або ext4 у віртуальній машині Linux.
Лишити їх змінними сесії. Постійний `setx` надішле наступний запуск на той самий том.

## ReFS

Dev Drive на ReFS, на VHDX. Windows 11 збірки 10.0.22621.2338 або новішої, 50 ГБ вільно і
права адміністратора. Кроки, які публікує Microsoft:
[Set up a Dev Drive on Windows 11](https://learn.microsoft.com/en-us/windows/dev-drive/).

Шлях у параметрах, як в інструкції Microsoft: System, Storage, Advanced storage settings,
Disks & volumes, Create Dev Drive, потім **Create new VHD**. Обрати VHDX, динамічне
розширення, не менше 50 ГБ (64 ГБ достатньо). Майстер форматує новий том як Dev Drive.
Форматувати його ще раз не треба.

З командного рядка, якщо VHDX не створює майстер, підвищений `diskpart` робить диск і літеру,
а формат, який позначає Dev Drive, — окрема команда. `maximum` задається в мегабайтах:

```text
create vdisk file="C:\Users\%USERNAME%\swarfr-refs.vhdx" maximum=65536 type=expandable
select vdisk file="C:\Users\%USERNAME%\swarfr-refs.vhdx"
attach vdisk
convert gpt
create partition primary
assign letter=R
```

Потім, усе ще з підвищенням, або:

```text
Format R: /DevDrv /Q
```

або в PowerShell:

```powershell
Format-Volume -DriveLetter R -DevDrive
```

Саме цей формат позначає том. Уже наявний том потім не перетворити. У прикладах літера `R:`;
підставити ту, яку отримав розділ.

Потім із клона:

```powershell
New-Item -ItemType Directory -Force -Path R:\swarfr-tmp | Out-Null
$env:TEMP = 'R:\swarfr-tmp'
$env:TMP = 'R:\swarfr-tmp'
just check
```

## Що перевіряє запуск

`caps` на Windows дорівнює `NONE` до T21, а `clone_file` повертає `ErrorKind::Unsupported`.
Контрактний тест `a_clone_holds_the_bytes_of_its_source_or_refuses_to_pretend` чекає на цю
помилку і на NTFS, і на ReFS. `clone_file`, який копіює байти, зробить тест зеленим, зламавши
контракт.

Обидва результати записати в картку T24 у `plan.md`. Завдання закінчене, коли туди записано
запуск на NTFS.

## Що натомість робить CI

`just check-cross` перевіряє типи для `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` і
`aarch64-pc-windows-msvc`. Остання ціль — та, на якій працює гість. CI не запускає набір
тестів на Windows.
