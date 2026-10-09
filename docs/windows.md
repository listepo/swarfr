# Windows tests

The suite runs in a local Windows virtual machine. CI type-checks the Windows backends and does
not execute the tests. This is the same kind of choice as the lima VM for the Linux backend.

## Guest

On Apple Silicon the guest is Windows 11 on ARM. [UTM](https://github.com/utmapp/UTM) is the
hypervisor. This repository does not install it, and nothing here is installed on the machine
that hosts the guest. A guest brought up some other way is the same procedure once Windows 11
on ARM is running.

Give the virtual machine a disk large enough for Windows, and room for a second dynamically
expanding VHDX of at least 50 GB. The system disk stays NTFS.

## The repository

Inside the guest, clone over the network:

```bash
git clone https://github.com/listepo/swarfr
cd swarfr
```

A folder shared from the Mac is not a substitute. The tests write their fixtures in the temp
directory, and that directory has to sit on the volume under test.

## Tools

The pin is `rust-toolchain.toml` (channel 1.98). Install mise the way its Windows instructions
describe, then in the clone:

```bash
mise install
```

`just check` is the suite. The recipe that follows the tests is a shell script, and it does
nothing when `swarfr` is not installed. It needs `sh` on `PATH`. Git for Windows provides it.
From PowerShell, for this session:

```powershell
$env:PATH = "C:\Program Files\Git\bin;$env:PATH"
```

## NTFS

The system drive is the NTFS volume. From the clone, for this session only:

```powershell
New-Item -ItemType Directory -Force -Path C:\swarfr-tmp | Out-Null
$env:TEMP = 'C:\swarfr-tmp'
$env:TMP = 'C:\swarfr-tmp'
just check
```

`TEMP` and `TMP` choose the volume, the way `TMPDIR` chose btrfs or ext4 in the Linux VM. Leave
them as session variables. A permanent `setx` would send the next run at the same volume.

## ReFS

A ReFS Dev Drive, on a VHDX. Windows 11 build 10.0.22621.2338 or later, 50 GB free, and an
administrator. The steps Microsoft publishes are
[Set up a Dev Drive on Windows 11](https://learn.microsoft.com/en-us/windows/dev-drive/).

Settings, System, Storage, Advanced storage settings, Disks & volumes, Create Dev Drive, then
**Create new VHD**. Choose VHDX, dynamically expanding, at least 50 GB (64 GB is enough). The
wizard formats the new volume as a Dev Drive. Do not format it again.

From a command line, when the VHDX is not created for you, an elevated `diskpart` makes the
disk and a letter, and the format that marks a Dev Drive is a separate command. `maximum` is
megabytes:

```text
create vdisk file="C:\Users\%USERNAME%\swarfr-refs.vhdx" maximum=65536 type=expandable
select vdisk file="C:\Users\%USERNAME%\swarfr-refs.vhdx"
attach vdisk
convert gpt
create partition primary
assign letter=R
```

Then, still elevated, either:

```text
Format R: /DevDrv /Q
```

or, in PowerShell:

```powershell
Format-Volume -DriveLetter R -DevDrive
```

That format is what marks the volume. An existing volume cannot be converted afterwards. The
letter in the samples is `R:`; use the letter the partition actually received.

Then, from the clone:

```powershell
New-Item -ItemType Directory -Force -Path R:\swarfr-tmp | Out-Null
$env:TEMP = 'R:\swarfr-tmp'
$env:TMP = 'R:\swarfr-tmp'
just check
```

## What the run is checking

`caps` on Windows is `NONE` until T21, and `clone_file` returns `ErrorKind::Unsupported`. The
contract test `a_clone_holds_the_bytes_of_its_source_or_refuses_to_pretend` expects that error
on NTFS and on ReFS. A `clone_file` that copies bytes would turn the test green by breaking the
contract.

Write both outcomes into the T24 card in `plan.md`. The task is finished when the NTFS run is
recorded there.

## What CI does instead

`just check-cross` type-checks `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` and
`aarch64-pc-windows-msvc`. The last of those is the target the guest runs. CI does not run the
suite on Windows.
