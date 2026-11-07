# Pulse

A macOS menu bar app (and Windows tray app) for people who keep many Claude Code
sessions and scheduled routines going in the Claude desktop app, and threads in
the Codex app. It shows at a glance what
runs and what needs you: a question, a block, a result to read, a command
left running in the background.

https://github.com/user-attachments/assets/b85c4ad9-4e7a-4367-b419-6594073a9225

- **Menu bar:** a coloured symbol and count per open state; a grey check when
  nothing needs you.
- **Panel:** *Open* (what runs now, then what needs you, oldest first),
  *Timeline* (today, what comes, what was missed) and *History* (every
  session, searchable). A click opens the session in its app (Claude, or Codex
  for a Codex thread).
- **Commands to check:** background commands that run long, run hot, hang or
  were forgotten.
- **Work that has not landed:** files a session left uncommitted, commits not
  pushed, a branch without a PR.
- **Notifications** for what newly needs you and reminders while it lies,
  as you set them; a click opens the session.
- Park a topic for later, mark things done or read, move the panel anywhere
  or shrink it to a bar of the counts; light and dark.

It has two parts:

| Part | What it does |
| --- | --- |
| [`routine-recorder`](routine-recorder) | A Claude Code plugin. It asks every session to end its answers with a `STATUS:` line (worked, waiting, blocked, idle, parked), which the app then does not show, records each scheduled-routine run to `~/.claude/routine-recorder/runs` and reports what waits for your approval and which API error ended a turn. |
| [`desktop`](desktop) | Pulse itself, a Tauri app. It reads the sessions of the Claude desktop app, their transcripts and the records of the plugin. |

## Windows and Codex

Pulse also runs on Windows 10/11, where it sits in the notification area (the
tray) and its panel opens by the taskbar. The tray shows one square: the most
urgent state's symbol with a badge of everything open; the tooltip names each
count (for example "Pulse: 1 blocked, 2 questions").
On Windows Pulse reads:

- the Claude app's sessions under `%LOCALAPPDATA%\Claude-Data\<package>` (the
  packaged app, the download and the Store build alike) or `%APPDATA%\Claude`
  (the old installer); when more than one holds sessions, Pulse reads the one
  written to last when it starts and stays with it, so after a switch to
  another build of Claude, quit and start Pulse again;
- the transcripts and the plugin's records under `%USERPROFILE%\.claude`;
- the commands sessions run, from the processes that hold their
  `%TEMP%\claude\…\tasks\<id>.output` (or the same under `CLAUDE_CODE_TMPDIR`
  or `%TMP%`) open, as the file system names them;
- its own state in `%APPDATA%\local.claude-dashboard.pulse`.

Pulse reads `CLAUDE_CODE_TMPDIR`, `TEMP` and `TMP` from its own environment, so
the commands of a Claude Code started with a different temp folder (set only in
its shell, or in Claude Code's settings) are not seen.

**Codex**, on macOS and Windows alike: Pulse shows the threads of the Codex app
(desktop or CLI) beside
Claude's sessions, marked *Codex*: their titles, folders, archive and (for the
app's threads) read state from `~/.codex/state_5.sqlite`, and from each thread's rollout in
`~/.codex/sessions` whether a turn runs now, its last tool and its last answer
(`~/.codex` is `%USERPROFILE%\.codex` on Windows, or wherever `CODEX_HOME` points).
Codex compresses the rollout of a thread idle for some days to
`rollout-….jsonl.zst`; Pulse reads that file as it reads the plain one. A
thread in a Codex project shows the project's name; a chat the Codex app
started outside any project, in a folder of its own under
`Documents\Codex\<date>` (on Windows also when Documents was moved or is in
OneDrive), shows no project.
A click opens the thread in Codex (`codex://threads/<id>`). Only threads you
started in the Codex app or CLI count, not subagents, reviews or threads another
tool drives through Codex. A finished turn in the Codex app is a result to
read until you open the thread there; in the CLI you read the answer in the
terminal, so it counts as done at once. An answer that ends in a question asks
you something, in the app or the CLI. For the same states as a
Claude session (waiting, blocked, parked), ask Codex for a STATUS line in
`~/.codex/AGENTS.md`:

```markdown
End every answer with one line `STATUS: worked|waiting|blocked|idle|parked | one sentence`:
worked when the task is done, waiting when you need the user, blocked when you cannot go on.
```

Codex keeps no record of approval prompts in its rollouts, so a Codex thread
waiting for an approval shows as running.

Build on Windows: Rust (rustup), Node 20+ and the Visual Studio C++ Build Tools,
then `npm run setup` and `npm run build` in `desktop`; the installer lands in
`desktop/src-tauri/target/release/bundle/nsis/`.

## Requirements

- macOS 11 or later, or Windows 10/11 (see [Windows and Codex](#windows-and-codex))
- the [Claude desktop app](https://claude.ai/download), its Code tab: Pulse
  reads the sessions and scheduled tasks the app keeps
- Claude Code on the command line (`claude`), to install the plugin

## Install

### 1. The plugin

In a terminal:

```bash
claude plugin marketplace add naphti88/pulse
claude plugin install routine-recorder@pulse
```

(or `/plugin marketplace add …` and `/plugin install …` in an interactive
`claude` session).

Sessions started after that end their answers with a `STATUS:` line; that line
is how Pulse knows whether a session needs you or only has something to read.
Without the plugin Pulse falls back on the app's own summaries.

### 2. The app

Download the `.dmg` (one app for Apple silicon and Intel) from the
[latest release](https://github.com/naphti88/pulse/releases/latest),
open it and drag Pulse to Applications.

The app is not signed with an Apple developer certificate, so macOS refuses to
open it the first time. Either clear the download flag once:

```bash
xattr -dr com.apple.quarantine /Applications/Pulse.app
```

or open it, then allow it under System Settings → Privacy & Security → *Open
Anyway*. Pulse lives in the menu bar (no Dock icon); a right click on its
symbol turns on *Start at login*.

On Windows, download `Pulse_<version>_x64-setup.exe` from the same release and
run it. It is not signed either, so SmartScreen says *Windows protected your
PC*: click *More info*, then *Run anyway*. It installs for your user under
`%LOCALAPPDATA%` without admin rights, and fetches the WebView2 runtime if it is
missing. Pulse then lives in the tray; a right click on it turns on *Start at
login*.

### Or build it yourself

You need Rust 1.90 or later (the toolchain is pinned in
`desktop/src-tauri/rust-toolchain.toml`, `rustup` fetches it), Node 20 or later
and the Xcode command line tools.

```bash
cd desktop
npm run setup
npm run build
```

The app is then at `desktop/src-tauri/target/release/bundle/macos/Pulse.app`.

### Optional: older routine runs

Runs from before the plugin was installed can be recorded from their
transcripts:

```bash
python3 -I tools/backfill_runs.py --dry-run
python3 -I tools/backfill_runs.py
```

On Windows, if `python3` is missing or only the Microsoft Store placeholder,
run `py -3 -I tools/backfill_runs.py --dry-run`, then the same without
`--dry-run`.

## What it reads and writes

Everything stays on your computer; neither Pulse nor the plugin sends anything
anywhere.

- **Reads:** the Claude desktop app's sessions and scheduled tasks
  (`~/Library/Application Support/Claude/claude-code-sessions`; on Windows see
  [Windows and Codex](#windows-and-codex)), a copy of its
  unread markers, the transcripts in `~/.claude/projects`, the plugin's records,
  Claude Code's records of its running processes (`~/.claude/sessions`), the
  Codex threads in `~/.codex/state_5.sqlite` and `~/.codex/sessions` (or
  `CODEX_HOME`), the process table (for the commands sessions run; on Windows
  the `claude` output files under `CLAUDE_CODE_TMPDIR`, `%TEMP%` and `%TMP%`
  instead) and `git` in the folders recent sessions worked in (to see work that
  has not landed). It never fetches, commits or pushes.
- **Writes:** its own state and settings in
  `~/Library/Application Support/local.claude-dashboard.pulse/` (Windows:
  `%APPDATA%\local.claude-dashboard.pulse`, and the panel's WebView2 profile in
  `%LOCALAPPDATA%\local.claude-dashboard.pulse`); the plugin
  writes its records to `~/.claude/routine-recorder/runs` and what waits for
  you, by the name of a tool only, to `~/.claude/routine-recorder/pending`.

Pulse reads files of the Claude desktop app that are not a documented
interface. An update of the app can change them and break Pulse until it is
adapted.

## Uninstall

Turn off *Start at login* and quit Pulse from its menu bar symbol (right
click → *Start at login*, then *Quit Pulse*), then delete
`/Applications/Pulse.app` and
`~/Library/Application Support/local.claude-dashboard.pulse` (and
`~/Library/LaunchAgents/Pulse.plist`, if Pulse was left to start at login).
Remove the plugin
with `claude plugin uninstall routine-recorder@pulse`, and its records
with `~/.claude/routine-recorder` if you like.

On Windows, quit Pulse from its tray icon (right click → *Quit Pulse*),
uninstall it under Settings → Apps → Installed apps (Windows 10: Apps &
features) → *Pulse*, which also removes *Start at login*, and tick *Delete the
application data*, which removes both
`%APPDATA%\local.claude-dashboard.pulse` and
`%LOCALAPPDATA%\local.claude-dashboard.pulse` (the WebView2 profile); or delete
both folders yourself. The plugin goes as above.

## Development

How Pulse works, what each state means and how to build and test it:
[`desktop/README.md`](desktop/README.md). The plugin's tests run with
`claude plugin test routine-recorder`; Pulse's with `npm test` in `desktop`,
and the panel's type check with `npm --prefix ui run typecheck` there.

Nothing private may enter this public repository: run `npm run setup` in
`desktop` once, it turns on the hooks that check every commit and push. How that
works: [Keeping what is private out of git](desktop/README.md#keeping-what-is-private-out-of-git).

## License

[MIT](LICENSE)
