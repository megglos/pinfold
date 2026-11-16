# CorePin: persistent per-process CPU affinity for Windows

CorePin is the old name of Pinfold. It is a small background tool. It keeps
selected programs on selected CPU cores each time they start.

Revision 2 adds: handling of affinity that other software changes (AMD
chipset driver, games, anti-cheat), and a strict resource budget. The stack
is native code, not .NET. A tray menu replaces the main window.

## 1. Problem

On dual-CCD Ryzen X3D CPUs (here: Ryzen 9 9950X3D, 16 cores, 32 threads), only
one CCD has 3D V-Cache. Windows and the AMD chipset driver sometimes put a
game on the frequency CCD. Windows cannot keep an affinity for each
executable. Task Manager settings stop when the process stops. Other software
can change an affinity that you set. Process Lasso does much more than
necessary.

## 2. Goals

1. Assign a running process to a set of logical cores.
2. Keep the assignment as a rule. Apply it each time the executable starts.
3. Named profiles (for example "Cache", "Frequency"): one click assigns a
   process.
4. Keep the assignment when other software changes it. Log each change.
5. Almost no resource use when idle (section 8).

## 3. Non-goals

- Priority classes, I/O priority, power plans.
- Changes to AMD driver settings, core parking or the 3D V-Cache service. The
  tool reads their state for diagnostics only.
- Affinity for each thread.
- Systems with more than 64 logical processors (processor groups).
- Windows services and system processes.
- Automatic game detection. The user makes all rules.
- A main window in v1.

## 4. Target environment

- Windows 11, x64. Windows 10 22H2 should also work.
- One processor group, 64 logical processors maximum.
- Test machine: Ryzen 9 9950X3D, Nvidia GPU, AMD chipset driver.

## 5. Concepts

**Profile**: a name and a set of logical processor indices.

**Rule**: a match pattern for an executable, a profile and a mode.

**Mode**: how the tool applies the profile.
- `affinity`: hard affinity mask (as in Task Manager).
- `cpusets`: process default CPU Sets. This is a different mechanism.
- `both`: the two together.

**Drift**: the affinity or CPU Sets of a matched process are not equal to the
values that the rule applied.

Match order (case-insensitive, first match wins):
1. Full path (`D:\Games\Foo\foo.exe`)
2. File name (`foo.exe`)

## 6. Functional requirements

### 6.1 Apply
- F1. When the user assigns a profile, apply it to the running process
  immediately.
- F2. An assignment makes or updates the rule for the executable.
- F3. Apply the rule to each new process that matches it.
- F4. When the user removes a rule, set the matched running processes to all
  cores.

### 6.2 New processes
- F5. Read the list of process IDs at a fixed interval (default 1000 ms,
  250 to 5000 ms) and compare it with the last list. Open and match only new
  PIDs.
- F6. Identify a handled process by PID and start time, because Windows
  reuses PIDs.

### 6.3 Enforcement and drift
- F7. Keep a handle open for each matched process. Read its affinity and CPU
  Sets at each poll. This costs a few calls for each matched process only.
- F8. On drift, apply again and log: time, executable, expected value, found
  value, time since the last apply.
- F9. Back-off: if a process drifts more than 5 times in 60 seconds, stop for
  that process instance. Mark the rule "contested" and show it in the tray
  menu. A fight with other software can cause stutter.
- F10. Enforcement is on by default. The user can set it off for each rule.
- F11. If the tool cannot open a process (access denied), log this one time
  for each instance and mark the rule "denied".

### 6.4 Profiles
- F12. At the first start, read the CPU topology and make these profiles:
  - "Cache" and "Frequency" if there are two L3 domains of different size.
    The larger L3 is the V-Cache CCD.
  - "All cores" always.
- F13. The user edits profiles in the config file as CPU ranges (`"0-15"`),
  not as hex masks.

### 6.5 UI: tray menu only
- F14. Tray icon with a menu. The tool makes the menu when the user opens it:
  - **Assign** > processes of the user that have a visible window, newest
    first, the foreground process at the top > profiles.
  - **Rules** > each rule with its profile and state (ok, contested, denied)
    > profile, mode, enforce, remove.
  - **Pause / resume**
  - **Open config**, **Open log**
  - **Start with Windows** (on/off)
  - **Exit**
- F15. No window before the user opens the menu. No background updates for
  the UI.

### 6.5a Rules for programs that do not run
- F20. **Drop on the executable.** Drop an `.exe` or a `.lnk` shortcut on
  `CorePin.exe` or on a shortcut to it. Windows starts a short instance with
  the path as argument. This instance reads the target of a `.lnk`, shows the
  profiles at the mouse position, writes the rule to the config and stops.
  The background instance reads the new config (F16). Thus the two instances
  do not communicate, and elevation of the background instance has no
  effect.
- F21. The command line does the same: `CorePin.exe add <path> [--profile Cache]`.
- F22. **Tray menu > Add program...** opens a file picker for `.exe` and
  `.lnk`, then the profiles.
- F23. Rules from the UI (running process, drop, file picker) match by file
  name. The user can change a rule to full path in the config.
- F24. The tool cannot read shortcuts that do not point to an executable:
  `.url` shortcuts (Steam `steam://rungameid/...`) and Store/UWP links. The
  tool tells the user to start the program and assign it from the running
  list.

The tray icon does not accept drops, because Windows does not send file drops
to tray icons. There is no drop window, because Windows blocks drops from
Explorer to an elevated program.

### 6.6 Persistence
- F16. One JSON file, `%AppData%\CorePin\config.json`. A person can edit it.
  The tool reads it again when it changes.
- F17. Atomic writes: write a temporary file, then replace.

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

### 6.7 Log
- F18. Text log next to the config, 1 MB maximum, one old file. Entries:
  applied, drift, contested, denied, config errors.
- F19. At start, log the environment one time: CPU topology, presence of the
  `amd3dvcache` driver and the `amd3dvcacheSvc` service, the values in
  `HKLM\SYSTEM\CurrentControlSet\Services\amd3dvcache\Preferences`, and Game
  Mode on or off. Read only.

## 7. Interference

What AMD's software probably does: when Windows finds a game, the chipset
driver changes the preferred CCD and parks the cores of the other CCD. It
uses core preference and parking, not affinity masks. Microsoft says that
Windows always obeys the affinity of a thread, also if it points to parked
cores.

Reports against this:
- A 9950X3D2 owner saw games with an affinity mask of cores 16-31 only. The
  owner did not set it.
- A community article says that the AMD PPM Provisioning driver can change
  manual masks. Not verified.
- Anti-cheat (reported: EA Javelin) can reset or ignore an affinity from
  outside, also an affinity from a launcher.
- Games can change their own affinity after the start.

Thus three sources can change the affinity: AMD drivers, the game, the
anti-cheat. We cannot know the cause without a test. The design:

1. Finds drift and logs it (F7, F8). The log shows if, when and how
   frequently drift occurs on this machine.
2. Supports CPU Sets and the affinity mask. Windows keeps them separately.
   Software that resets the mask can possibly leave the CPU Sets. M0 must
   test this.
3. Stops instead of a fight (F9).
4. Does not change AMD drivers, services or registry values.

## 8. Resource budget

Targets for acceptance:

| Item | Target |
|---|---|
| Private memory, idle | less than 5 MB |
| CPU, idle with 5 rules and 2 matched processes | less than 0.1 % of one core, not visible in Task Manager |
| Executable size | less than 2 MB, no runtime |
| Disk writes when nothing occurs | none |
| Threads | 1 worker and the tray message loop |

How the design meets the targets:
- Native code. No managed runtime, no UI framework.
- At each poll: one call for the PID list, a compare, and work for new PIDs
  only. The tool reads the path of a process one time.
- Enforcement uses only matched processes, through handles that are open.
- The tool makes the tray menu when the user opens it.
- The tool writes the log only for events.

Optional later step: use kernel process-start events (ETW) instead of polls.
This removes the periodic wake-up, but the tool must then run elevated.

## 9. Technical design

### 9.1 Stack
- Rust with the `windows` crate. One binary. Win32 tray icon and popup menu.
  `serde_json` for the config.
- Rust toolchain with the MSVC target (`x86_64-pc-windows-msvc`).
- Few dependencies. Compare each new crate with the targets of section 8.

### 9.2 Windows APIs
| Purpose | API |
|---|---|
| PID list | `EnumProcesses` |
| Executable path, start time | `QueryFullProcessImageNameW`, `GetProcessTimes` (`PROCESS_QUERY_LIMITED_INFORMATION`) |
| Hard affinity | `SetProcessAffinityMask`, `GetProcessAffinityMask` (`PROCESS_SET_INFORMATION`) |
| CPU Sets | `GetSystemCpuSetInformation`, `SetProcessDefaultCpuSets`, `GetProcessDefaultCpuSets` (`PROCESS_SET_LIMITED_INFORMATION`) |
| Topology | `GetLogicalProcessorInformationEx` with `RelationCache` (L3 size and mask) and `RelationProcessorCore` (SMT pairs) |
| Process exit | wait on the process handle (`SYNCHRONIZE`) or check at each poll |
| Tray | `Shell_NotifyIconW`, `TrackPopupMenu` |
| Config changes | `ReadDirectoryChangesW` or file time check at each poll |

The topology always comes from the API. Do not assume a core numbering.

### 9.3 Privileges
- Without elevation, the tool can pin processes of the same user at the same
  integrity level.
- If a game runs elevated, the tool must also run elevated.
- The background instance runs elevated by default. "Start with Windows"
  makes a Task Scheduler logon task with the highest rights. Thus there is no
  UAC prompt at logon. The task needs one UAC prompt when the tool makes it.
- A config option gives the variant without elevation (`HKCU\...\Run`).
- The short "add" instance (F20, F21) runs without elevation and only writes
  the config.

### 9.4 Structure
- `topology`: L3 domains, SMT pairs, default profiles.
- `config`: load, validate, save, reload.
- `engine`: poll, match, apply, drift check, back-off. No UI. Tests use a
  fake process source.
- `diag`: environment at start.
- `tray`: icon and menu.

## 10. Risks

1. **Other software changes the pin again.** See section 7. Drift log, CPU
   Sets and back-off decrease the risk. M0 finds the cause.
2. **Anti-cheat.** Protected processes can refuse the handle. Anti-cheat can
   reset the pin. Some players think that affinity tools can cause a ban.
   This is a forum claim, not confirmed. Do not make rules for multiplayer
   games with kernel anti-cheat.
3. **Hard affinity and parking.** A game on the parked Frequency CCD should
   work, because Windows obeys affinity on parked cores. Test this on this
   machine.
4. **Start delay.** The pin comes up to one poll interval after the process
   starts. This is probably not a problem.

## 11. Acceptance criteria

- A1. Assign `notepad.exe` to "Frequency". Task Manager shows the affinity.
  Start Notepad again. In 2 seconds, the pin is back without user action.
- A2. After a reboot, the tool runs and A1 is true.
- A3. On the 9950X3D, the first start makes "Cache" and "Frequency". "Cache"
  has the 16 logical processors of the CCD with the larger L3.
- A4. Change the affinity of a pinned process in Task Manager. In one poll,
  the tool sets it back and logs a drift entry.
- A5. Change it 6 times in one minute. The rule becomes "contested" and the
  tool stops for that process instance.
- A6. The tool logs a process that it cannot open one time and shows it as
  "denied". The tool continues to run.
- A9. Close Notepad. Drop `notepad.exe` on `CorePin.exe`, then a shortcut to
  it. Select "Frequency". The rule is in the tray menu and the tool pins the
  next Notepad. A Steam `.url` shortcut gives a message and no rule.
- A7. The tool meets the targets of section 8 for 10 minutes idle.
- A8. A real game with a "Cache" rule uses only the cache CCD for 30 minutes
  (Task Manager, view for each core). The log shows no drift or a known
  pattern.

## 12. Milestones

0. **M0, interference test (console, no tray).** Topology, apply, drift log
   only. Run two or three games that have problems now. Use mode `affinity`,
   then `cpusets`, then `both`, then one time with Game Mode off. Result:
   which mode holds, how frequently drift occurs, the default mode.
1. **M1, engine.** Config, rules, enforcement, back-off, log. Proves A1, A3
   to A6.
2. **M2, tray.** Menu, assign, rule states, autostart, add by drop, file
   picker or command line. Proves A2, A9.
3. **M3, measurement.** A7, A8, poll interval.

## 13. Decisions

No open decisions. Decided:
- The default mode for new rules is `affinity`. Change a rule to `both` if
  the log shows drift for that program. Change it to `cpusets` if the program
  has problems with a hard mask. M0 can change this default.
- The UI is the tray menu only. No main window.
