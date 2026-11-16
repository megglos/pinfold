# CorePin: persistent per-process CPU affinity for Windows

Working name: CorePin. A tiny background tool that pins chosen executables to a chosen set of CPU cores, every time they run.

Revision 2: adds handling for affinity being overridden (AMD chipset driver, games, anti-cheat) and a strict resource budget. The stack changed from .NET to native code and the main window was dropped in favour of a tray menu.

## 1. Problem

On dual-CCD Ryzen X3D CPUs (here: Ryzen 9 9950X3D, 16 cores / 32 threads), only one CCD has 3D V-Cache. Windows and the AMD chipset driver sometimes schedule a game onto the frequency CCD instead. Windows has no built-in way to store a per-executable affinity, Task Manager settings are lost when the process exits, and affinity set by hand has been observed to be overridden again. Process Lasso is far more than is needed.

## 2. Goals

1. Assign a running process to a set of logical cores.
2. Persist that assignment as a rule and apply it automatically on every future start of the same executable.
3. Named affinity profiles (e.g. "Cache", "Frequency") so assigning a process is one click.
4. Keep the assignment in place when something else changes it, and record that it happened.
5. Negligible resource use while idle (see section 8).

## 3. Non-goals

- Priority classes, I/O priority, power plans.
- Changing AMD driver settings, core parking, or the 3D V-Cache service. The tool reads their state for diagnostics only.
- Per-thread affinity.
- Systems with more than 64 logical processors (processor groups).
- Windows services and system processes.
- Automatic "is this a game?" detection. Rules are explicit.
- A main window, in v1.

## 4. Target environment

- Windows 11 (Windows 10 22H2 should also work), x64.
- Single processor group, up to 64 logical processors.
- Primary test machine: Ryzen 9 9950X3D, Nvidia GPU, AMD chipset driver installed.

## 5. Concepts

**Profile**: a name plus a set of logical processor indices.

**Rule**: a match pattern for an executable, the profile to apply, and a mode.

**Mode**: how the profile is applied.
- `affinity`: hard affinity mask (what Task Manager sets).
- `cpusets`: process default CPU Sets, a separate mechanism from the affinity mask.
- `both`: apply both.

**Drift**: a matched process whose current affinity or CPU Sets no longer equal what the rule applied.

Matching, case-insensitive, first match wins:
1. Full path (`D:\Games\Foo\foo.exe`)
2. File name (`foo.exe`)

## 6. Functional requirements

### 6.1 Applying
- F1. Apply a profile to a running process immediately when the user assigns it.
- F2. Assigning a profile to a process creates or updates a rule for its executable.
- F3. Every newly started process that matches a rule gets the rule applied.
- F4. Removing a rule resets running matched processes to all cores.

### 6.2 Detection of new processes
- F5. Poll the list of process IDs on a fixed interval (default 1000 ms, configurable 250 to 5000 ms) and diff against the previous list. Only new PIDs are opened and matched.
- F6. Track handled processes by PID plus start time, to survive PID reuse.

### 6.3 Enforcement and drift (new)
- F7. For each matched running process, keep its handle open and re-read affinity and CPU Sets on every poll. This is a few cheap calls per matched process, not per system process.
- F8. On drift, re-apply and log: timestamp, executable, expected value, value found, time since last apply.
- F9. Back-off: if a process drifts more than 5 times in 60 seconds, stop re-applying for that process instance, mark the rule "contested", and show it in the tray menu. This avoids a fight that could cause stutter.
- F10. Enforcement is on by default and can be turned off per rule.
- F11. A process that refuses the handle (access denied) is logged once per instance and marked "denied" on its rule.

### 6.4 Profiles
- F12. On first run, read the CPU topology and create profiles:
  - "Cache" and "Frequency" when two L3 domains of different size exist (the larger L3 is the V-Cache CCD).
  - "All cores" always.
- F13. Profiles are edited in the config file as CPU ranges (`"0-15"`). No hex masks.

### 6.5 UI: tray menu only
- F14. Tray icon with a menu, built when opened:
  - **Assign** > list of the current user's processes that own a visible window, most recently started first > profile list. The foreground window's process is listed first.
  - **Rules** > each rule with its profile and state (ok, contested, denied) > change profile, change mode, toggle enforce, remove.
  - **Pause / resume**
  - **Open config**, **Open log**
  - **Start with Windows** (toggle)
  - **Exit**
- F15. No window is created until the menu is opened, and nothing is refreshed in the background for UI purposes.

### 6.5a Adding rules for programs that are not running
- F20. **Drop onto the executable.** Dropping an `.exe` or a `.lnk` shortcut onto `CorePin.exe` (or onto a desktop shortcut to it) starts it with that path as argument. This short-lived instance resolves `.lnk` files to their target, shows a profile picker at the mouse position, writes the rule into the config and exits. The background instance picks it up through the config reload (F16), so no communication between the two is needed and it works whether or not the background instance is elevated.
- F21. The same path works from the command line: `CorePin.exe add <path> [--profile Cache]`.
- F22. **Tray menu > Add program...** opens a standard file picker for `.exe` and `.lnk`, then the profile list.
- F23. Rules created from the UI (running process, drop, file picker) match by file name by default; switching a rule to full-path matching is done in the config.
- F24. Shortcuts that do not point at an executable cannot be resolved: `.url` shortcuts such as Steam's `steam://rungameid/...`, and Store/UWP app links. The tool says so and suggests starting the program once and assigning it from the running list.

Dropping onto the tray icon is not offered: Windows does not deliver file drops to tray icons. A drop window is avoided because drops from Explorer are blocked when the receiving program runs elevated.

### 6.6 Persistence
- F16. One JSON file at `%AppData%\CorePin\config.json`, human-editable, reloaded when it changes on disk.
- F17. Atomic writes (temp file, then replace).

```json
{
  "pollIntervalMs": 1000,
  "profiles": [
    { "name": "Cache",     "cpus": "0-15"  },
    { "name": "Frequency", "cpus": "16-31" }
  ],
  "rules": [
    { "match": "cyberpunk2077.exe", "profile": "Cache", "mode": "affinity", "enforce": true },
    { "match": "D:\\Tools\\encoder.exe", "profile": "Frequency", "mode": "affinity", "enforce": true }
  ]
}
```

### 6.7 Logging
- F18. Plain text log next to the config, size-capped (1 MB, one rotation). Entries: applied, drift, contested, denied, config errors.
- F19. At startup, log the environment once: CPU topology, whether the `amd3dvcache` driver and `amd3dvcacheSvc` service are present, the values under `HKLM\SYSTEM\CurrentControlSet\Services\amd3dvcache\Preferences`, and whether Windows Game Mode is on. Read-only.

## 7. Interference: what is known and how the tool deals with it

What AMD's stack is understood to do: when Windows detects a game, the chipset driver changes which CCD is preferred and parks the other CCD's cores. That is steering through core preference and parking, not through affinity masks. Microsoft's description of core parking is that a thread's affinity is always honoured, even if it points at parked cores.

What contradicts the simple picture:
- A 9950X3D2 owner reported games whose affinity mask showed cores 16-31 only, without having set it.
- A community write-up claims AMD's PPM Provisioning driver can override manually set masks. Unverified.
- Anti-cheat (EA Javelin is the reported case) is said to reset or ignore affinity set from outside, including affinity inherited from a launcher.
- Games can also change their own affinity after start.

So an override is plausible from three sources (AMD driver stack, the game, anti-cheat) and the cause cannot be settled on paper. The design therefore:

1. Detects drift and logs it (F7, F8), which shows whether, when and how often it happens on this machine.
2. Supports CPU Sets in addition to the affinity mask. They are stored separately, so whatever resets the mask may leave CPU Sets alone. This is a hypothesis to test in M0, not a guarantee.
3. Backs off instead of fighting (F9).
4. Never modifies AMD's driver, service or registry preferences.

## 8. Resource budget

Targets, to be measured in acceptance:

| Item | Target |
|---|---|
| Private memory, idle | under 5 MB |
| CPU, idle with 5 rules and 2 matched processes | under 0.1 % of one core, not visible in Task Manager |
| Executable size | under 2 MB, no runtime to install |
| Disk writes when nothing happens | none |
| Threads | 1 worker plus the tray message loop |

How the design meets it:
- Native code, no managed runtime, no UI framework.
- Per poll: one call for the PID list, a diff, and work only for new PIDs. Executable paths are queried once per new process.
- Enforcement touches only matched processes through handles that are already open.
- Tray menu contents are computed on demand.
- The log is written only on events.

Optional later step if polling is still considered too much: replace polling with kernel process-start events (ETW). It removes the periodic wake-up but requires the tool to run elevated.

## 9. Technical design

### 9.1 Stack
- Decided: Rust with the `windows` crate, one binary, Win32 tray icon and popup menu. `serde_json` for the config.
- Prerequisite: Rust toolchain with the MSVC target (`x86_64-pc-windows-msvc`).
- Keep dependencies few; every added crate is weighed against the size and memory targets of section 8.

### 9.2 Windows APIs
| Purpose | API |
|---|---|
| PID list | `EnumProcesses` |
| Executable path, start time | `QueryFullProcessImageNameW`, `GetProcessTimes` (`PROCESS_QUERY_LIMITED_INFORMATION`) |
| Hard affinity | `SetProcessAffinityMask`, `GetProcessAffinityMask` (`PROCESS_SET_INFORMATION`) |
| CPU Sets | `GetSystemCpuSetInformation`, `SetProcessDefaultCpuSets`, `GetProcessDefaultCpuSets` (`PROCESS_SET_LIMITED_INFORMATION`) |
| Topology | `GetLogicalProcessorInformationEx` with `RelationCache` (L3 size and mask) and `RelationProcessorCore` (SMT pairs) |
| Process exit | wait on the process handle (`SYNCHRONIZE`) or check on poll |
| Tray | `Shell_NotifyIconW`, `TrackPopupMenu` |
| Config changes | `ReadDirectoryChangesW` or timestamp check on poll |

Topology always comes from the API, never from assumed core numbering.

### 9.3 Privileges
- Unelevated, the tool can pin processes of the same user at the same integrity level.
- Games that run elevated need the tool elevated too.
- Decided: the background instance runs elevated by default. "Start with Windows" creates a Task Scheduler logon task with highest privileges, so there is no UAC prompt per logon. Creating the task needs one elevation prompt.
- An unelevated variant (`HKCU\...\Run`) stays available as a config option.
- The short-lived "add" instance (F20, F21) runs unelevated and only writes the config.

### 9.4 Structure
- `topology`: L3 domains, SMT pairs, default profiles.
- `config`: load, validate, save, reload.
- `engine`: poll, match, apply, drift check, back-off. No UI dependencies; testable with a fake process source.
- `diag`: startup environment snapshot.
- `tray`: icon and menu.

## 10. Risks

1. **Something keeps overriding the pin.** See section 7. Mitigated by drift logging, CPU Sets and back-off; resolved by the M0 experiment.
2. **Anti-cheat.** Protected processes can refuse the handle, and an anti-cheat may reset the pin. Some players also consider external affinity tools a ban risk in such titles; that is a claim from forum users, not confirmed. Recommendation: do not create rules for multiplayer titles with kernel anti-cheat.
3. **Hard affinity versus parking.** Pinning a game to the Frequency CCD while the driver parks it should still work, since affinity is honoured on parked cores, but needs a check on this machine.
4. **Start delay.** The pin lands up to one poll interval after process start. Expected to be harmless.

## 11. Acceptance criteria

- A1. Assign `notepad.exe` to "Frequency"; Task Manager shows the matching affinity. Restart Notepad; within 2 seconds the pin is back with no user action.
- A2. After a reboot the tool is running and A1 still holds.
- A3. On the 9950X3D, first run creates "Cache" and "Frequency", and "Cache" covers the 16 logical processors of the CCD with the larger L3.
- A4. Change a pinned process's affinity in Task Manager; within one poll it is restored and a drift entry is logged.
- A5. Change it 6 times within a minute; the rule becomes "contested" and re-applying stops for that process instance.
- A6. A process that cannot be opened is logged once and shown as "denied"; the tool keeps running.
- A9. With Notepad closed, drop `notepad.exe` and then a shortcut to it onto `CorePin.exe`, pick "Frequency"; the rule appears in the tray menu and the next Notepad start is pinned. Dropping a Steam `.url` shortcut gives the explanatory message and creates no rule.
- A7. Resource targets of section 8 are met, measured over 10 minutes idle.
- A8. A real game with a "Cache" rule shows load only on the cache CCD in Task Manager's per-core view for a 30 minute session, and the log shows either no drift or a named pattern.

## 12. Milestones

0. **M0, interference experiment (console, no tray).** Topology, apply, drift logging only. Run two or three games that misbehave today, each with mode `affinity`, then `cpusets`, then `both`, and once more with Game Mode off. Outcome: which mode holds, how often drift occurs, default mode for rules.
1. **M1, engine.** Config, rules, enforcement, back-off, log. Proves A1, A3 to A6.
2. **M2, tray.** Menu, assign, rule states, autostart, add by drop / file picker / command line. Proves A2, A9.
3. **M3, measurement.** A7, A8, tune poll interval.

## 13. Open decisions

None blocking. Decided so far:
- Default mode for new rules is `affinity`. A rule is switched to `both` when the log shows drift for that program, or to `cpusets` when the program misbehaves under a hard mask. M0 checks whether this default needs revisiting.
- UI is the tray context menu only. No main window is planned.
