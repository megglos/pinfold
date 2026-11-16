# CorePin

Persistent per-process CPU affinity for Windows. A tiny tray tool that pins
chosen executables to a chosen set of CPU cores every time they run, for
example a game to the V-Cache CCD of a dual-CCD Ryzen X3D.

The full specification is in [docs/SPEC.md](docs/SPEC.md).

## Build

Needs the Rust toolchain with the MSVC target and the Visual Studio C++ Build Tools.

```
cargo build --release
```

The result is `target\release\CorePin.exe`, a single file with no runtime to install.

## Use

```
CorePin.exe                          background instance with tray icon
CorePin.exe add <exe> [--profile <name>] [--mode affinity|cpusets|both] [--full-path]
CorePin.exe remove <exe>
CorePin.exe list
CorePin.exe topology
CorePin.exe run                      engine in a console, no tray
```

Dropping an `.exe` onto `CorePin.exe` shows a profile picker and writes the rule.

The tray menu offers: assign a running program to a profile, edit or remove
rules, add a program by file picker, pause, open config and log, start with
Windows.

Config and log live in `%AppData%\CorePin` (`config.json`, `corepin.log`).
The config is reloaded when it changes on disk. Profiles are created on first
run from the CPU topology: "Cache" and "Frequency" when two L3 domains of
different size exist, and "All cores".

Games that run elevated need CorePin to run elevated too; otherwise the rule
shows as "denied".

## Status

First milestone. Implemented: topology, profiles, rules, polling, apply
(affinity mask, CPU Sets, both), drift detection with back-off, logging,
startup diagnostics, tray menu, command line.

Not yet implemented from the spec:

- `.lnk` shortcut resolution (F20, F22)
- elevated autostart through Task Scheduler (9.3); "Start with Windows" uses the unelevated `HKCU\...\Run` variant
- foreground window listed first in Assign (F14)
- start-time check against PID reuse for unmatched processes (F6); matched processes are safe because their handle stays open
- engine tests against a fake process source (9.4)
- own icon (the default application icon is used)
