# Changelog

The release pipeline copies the section of a version into its release notes.
Write the section before you push the tag.

## 1.0.0-beta3 - 2026-11-16

Maintenance release. No new functions and no changes to the config.

- **Message boxes come to the front.** The benchmark messages (errors,
  "Restart elevated?") now come to the front, also when another window has
  the focus.
- **Internal changes.** The tray code is in smaller modules. Dialogs,
  rule changes and the benchmark report use shared helpers. There are new
  unit tests for the benchmark report.

### Install

Update from an earlier version:

1. Download `Pinfold-1.0.0-beta3-x64.zip`.
2. In the tray menu, select "Exit".
3. Replace `Pinfold.exe` with the new file.
4. Start `Pinfold.exe`.

For a new install, see the [README](https://github.com/megglos/pinfold#install).
For the benchmark, you need
[PresentMon](https://github.com/GameTechDev/PresentMon/releases).

## 1.0.0-beta2 - 2026-11-16

- **Own benchmark sounds.** Four short tones replace the Windows sounds:
  two rising notes at the start, a soft blip at each switch, two falling
  notes for a problem, and a rising chord at the end. To hear them, run
  `Pinfold.exe sounds`.

### Install

Update from 1.0.0-beta1:

1. Download `Pinfold-1.0.0-beta2-x64.zip`.
2. In the tray menu, select "Exit".
3. Replace `Pinfold.exe` with the new file.
4. Start `Pinfold.exe`.

For a new install, see the [README](https://github.com/megglos/pinfold#install).
For the benchmark, you need
[PresentMon](https://github.com/GameTechDev/PresentMon/releases).

## 1.0.0-beta1 - 2026-11-16

The first public version. Pinfold keeps games and other programs on the CPU
cores that you select, each time they start. Example: a game on the V-Cache
CCD of a dual-CCD Ryzen X3D.

### Functions

- **Rules for each program.** Assign a running program to a profile from the
  tray menu. Or drop an `.exe` or a `.lnk` shortcut on `Pinfold.exe`. Or use
  the file picker or `Pinfold.exe add`.
- **Profiles from the CPU topology.** At the first start, Pinfold makes
  "Cache", "Frequency" and "All cores" from the L3 domains of the CPU.
- **Three modes.** Hard affinity mask, CPU Sets, or both.
- **Enforcement.** If other software changes the cores of a pinned program,
  Pinfold sets them back and logs the change. After 5 changes in 60 seconds,
  Pinfold stops for that process and marks the rule "contested".
- **Start with Windows, elevated.** A Task Scheduler logon task starts
  Pinfold with the highest rights, without a UAC prompt at logon. Thus
  Pinfold can also pin games that run elevated.
- **Benchmark.** Compare profiles for a running game with
  [PresentMon](https://github.com/GameTechDev/PresentMon/releases), from
  the tray menu. Pinfold gives a verdict, tells you if the game is GPU-bound,
  and can make the winner the rule.
- **Small.** Less than 3 MB of memory and less than 0.02 % CPU when idle. One
  0.5 MB executable without a runtime.

### Install

1. Download `Pinfold-1.0.0-beta1-x64.zip` and extract `Pinfold.exe` to a
   folder, for example `%LocalAppData%\Programs\Pinfold`.
2. Start `Pinfold.exe`. The Pinfold icon comes into the tray.
3. In the tray menu, select "Start with Windows". You get one UAC prompt.

For the benchmark, download the
[PresentMon](https://github.com/GameTechDev/PresentMon/releases) console
application and put it next to `Pinfold.exe`.

### Known limits

- Beta: the tray menu and the dialogs have no automated tests.
- The executable has no signature. Windows SmartScreen can show a warning.
  Select "More info", then "Run anyway".
- Pinfold can miss a program that starts in the same poll interval in which
  an unmatched process with the same PID stopped.
- Not tested with anti-cheat. Do not make rules for multiplayer games with
  kernel anti-cheat.
- Windows only, x64, 64 logical processors maximum.
