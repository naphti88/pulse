# Pulse

A macOS menu bar app for people who keep many Claude Code sessions and
scheduled routines going in the Claude desktop app. It shows at a glance what
runs and what needs you: a question, a block, a result to read, a command
left running in the background.

https://github.com/user-attachments/assets/b85c4ad9-4e7a-4367-b419-6594073a9225

- **Menu bar:** a coloured symbol and count per open state; a grey check when
  nothing needs you.
- **Panel:** *Open* (what runs now, then what needs you, oldest first),
  *Timeline* (today, what comes, what was missed) and *History* (every
  session, searchable). A click opens the session in Claude.
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

## Requirements

- macOS 11 or later
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

## What it reads and writes

Everything stays on your Mac; neither Pulse nor the plugin sends anything
anywhere.

- **Reads:** the Claude desktop app's sessions and scheduled tasks
  (`~/Library/Application Support/Claude/claude-code-sessions`), a copy of its
  unread markers, the transcripts in `~/.claude/projects`, the plugin's records,
  Claude Code's records of its running processes (`~/.claude/sessions`), the
  process table (for the commands sessions run) and `git` in the folders recent
  sessions worked in (to see work that has not landed). It never fetches, commits
  or pushes.
- **Writes:** its own state and settings in
  `~/Library/Application Support/local.claude-dashboard.pulse/`; the plugin
  writes its records to `~/.claude/routine-recorder/runs` and what waits for
  you, by the name of a tool only, to `~/.claude/routine-recorder/pending`.

Pulse reads files of the Claude desktop app that are not a documented
interface. An update of the app can change them and break Pulse until it is
adapted.

## Uninstall

Quit Pulse from its menu bar symbol (right click → *Quit Pulse*), delete
`/Applications/Pulse.app` and
`~/Library/Application Support/local.claude-dashboard.pulse`. Remove the plugin
with `claude plugin uninstall routine-recorder@pulse`, and its records
with `~/.claude/routine-recorder` if you like.

## Development

How Pulse works, what each state means and how to build and test it:
[`desktop/README.md`](desktop/README.md). The plugin's tests run with
`claude plugin test routine-recorder`; Pulse's with `npm test` in `desktop`.

Nothing private may enter this public repository: run `npm run setup` in
`desktop` once, it turns on the hooks that check every commit and push. How that
works: [Keeping what is private out of git](desktop/README.md#keeping-what-is-private-out-of-git).

## License

[MIT](LICENSE)
