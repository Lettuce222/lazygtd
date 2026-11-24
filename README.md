# lazygtd · Rust preview

lazygtd is a standalone terminal GTD desk inspired by lazygit. It does not need
Go, Python, dotfiles, Hub, Linear or a separately installed SQLite. SQLite
3.54.1 is bundled in the executable. This preview neither reads nor migrates
legacy GTD files; it starts with a separate empty database.

## Run a prebuilt preview

Download the archive for your terminal from [GitHub Releases](https://github.com/Lettuce222/lazygtd/releases). No local compiler is required.

| Terminal | Archive platform suffix |
| --- | --- |
| Windows x64, including native PowerShell | windows-x86_64.zip |
| Windows on ARM64 | windows-arm64.zip |
| Linux x64, including Ubuntu WSL | linux-x86_64.tar.gz |
| Linux ARM64, including Ubuntu WSL | linux-aarch64.tar.gz |
| macOS on Apple Silicon | macos-arm64.tar.gz |
| macOS on Intel | macos-x86_64.tar.gz |

Compare SHA-256 with `SHA256SUMS` before extracting. Use
`Get-FileHash -Algorithm SHA256 PATH` in PowerShell, `sha256sum PATH` in
Linux/WSL, or `shasum -a 256 PATH` on macOS. Run `./lazygtd --demo` on
Linux/macOS or `.\lazygtd.exe --demo` in PowerShell. Release binaries are
not signed or notarized; checksums do not override platform trust prompts.

## Profiles and commands

- `--demo` loads 46 synthetic Japanese sample tasks covering all eight boxes into an in-memory SQLite database.
  Everything is discarded on exit, and no disk profile is created.
- The default work profile is persistent: Windows
  `%LOCALAPPDATA%\lazygtd\work.sqlite3`, Linux/WSL
  `${XDG_DATA_HOME:-$HOME/.local/share}/lazygtd/work.sqlite3`, macOS
  `~/Library/Application Support/lazygtd/work.sqlite3`.
- Choose a separate database with `--db PATH` before the command. Put it on
  a local filesystem, not a shared/network drive or synchronized folder.
  For a file backup, close all lazygtd processes first. SQLite may create
  adjacent `-wal` and `-shm` files while a process is running. On Unix new
  databases are owner-only; for older profiles close the app and run
  `chmod go-rwx` on the DB and any journal files before reopening. On Windows
  use the default protected local profile, or a private user-owned directory.
  There is no database encryption.
- With no command, open the TUI. Use a terminal of at least 66 columns and
  27 rows. `1`–`8`, `h`/`l` or Tab/Shift+Tab select a box; `j`/`k`
  select a row (wrapping at the ends); `c` opens an Inbox draft: first line title,
  following lines body. Ctrl+J or Shift+Enter inserts a line (if the terminal
  distinguishes it); Enter saves. `e` edits the body, `/` filters the current
  box by title/body, and Ctrl+D/U or Page Down/Up scrolls the detail.
  `f` then a number moves the task; `x`
  marks it done; `r` reloads; `?` shows help; `q` closes help or quits.
  Finished items are available from the CLI, not shown in active TUI boxes.
  `Enter` focuses the instruction/reply box, `p` manages project steps, `t`
  edits planning fields, `!` opens questions, and `o` shows artifacts.
  See the [Japanese demo walkthrough](docs/spec-progress.md).

```text
lazygtd add "Check tomorrow's plan" "Decide on the first step"
lazygtd list --json
lazygtd show 1
lazygtd move 1 1 next
lazygtd done 1 2
```

Mutations require the item's current revision except for add. A stale
revision fails with exit code 3. Reload with `show` or `list` before
deciding whether to retry. The item change and its event history are
committed together. There is no account, synchronization, legacy data import,
encryption, or Linear integration in this preview.

## Maintain or stage a release

```sh
cargo test --locked
cargo build --release --locked
```

The build compiles the bundled C SQLite sources with the developer/CI
toolchain; this is not a user-side dependency. The tag-triggered Release
workflow tests and builds only this repo. A maintainer tags a reviewed
commit with its Cargo version (e.g. `v0.1.0-preview.3`); the workflow
publishes a prerelease after the full platform matrix passes. It includes
SHA256SUMS, bundled notices and GitHub build provenance, not OS signatures. The previous Go implementation is [in dotfiles history](https://github.com/Lettuce222/dotfiles/tree/322bea95fea96e491cc1c742e32a92c231c5b61f/tools/lazygtd).

### Agent handoff (preview)

GTD box, assignee and agent run are independent. In Next, `AI ·` marks an
agent's task. In the TUI, `a` switches assignee and `i` saves directions without
sending anything. Try `lazygtd --demo`, press `4`, select the AI task, then press
`s`. Or press Enter and write a follow-up directly in the reply box. This uses a fake agent, returns the task to `1 あなたの番`, and keeps it open
for your review.

For a real run, make a local JSON file, e.g. `agent.json`:

```json
{"provider":"codex","executable":"/home/me/.local/bin/codex","workspace":"/home/me/src/project"}
```

Use `"claude"` instead of `"codex"` to select Claude Code, and optionally add a
`"model"`. The workspace must be an existing absolute path you trust. Install
and authenticate the official tool separately; configure the absolute path to the trusted official executable. Verify which
account you are using. Use a trusted official CLI and workspace; the provider's own settings and
capabilities are outside lazygtd's isolation boundary. File tools are not
requested by default; `allow_file_reads: true` explicitly requests read-only
file tools. Don't use this as a sandbox or send work data without authorization.
There is no automatic launch or credential registration.

1. Set assignee and directions with TUI, or run `assign ID REV agent` and
   `instruct ID REV "directions"` (each edit increments the revision).
2. `lazygtd agent-preview ID agent.json` prints the destination, workspace, revision
   and exact outbound prompt without connecting.
3. `lazygtd agent-run ID REV agent.json --send` submits the exact preview.
   Queued conversation includes the task body and recent previous turns.
   The TUI can also be opened with `--agent-config agent.json`, with an outgoing
   preview followed by a separate explicit confirmation. Do not include secrets or confidential
   material you are not authorized to send. Use the preview's revision.
4. `lazygtd agent-status ID` in another terminal or the TUI detail pane shows
   current activity and then the report or failure. With `--db PATH`, pass
   the same DB to each command.

The worker cannot mark a GTD task complete via its response. The app reserves
at most one active run per task; the exit status and provider protocol
determine the result. Both a report and a failure hand the task back to
the human review box. Fresh setup and existing v1–v4 databases use the same v5
schema; older binaries cannot open a migrated database. See [storage design and spec progress](docs/spec-progress.md).

These adapters are for read-only research/planning in a trusted workspace;
they are not OS isolation for arbitrary or untrusted repositories. They
request the provider's read-only tool restrictions, do not include write,
push or external-post adapters, and do not bypass permission prompts.
Actual worktree edits, provider session resume, external approval execution, trigger automation and import
from the old Hub are not in this preview. Real runs are foreground in the
launching terminal; a forced process shutdown may leave an active record
that needs manual recovery. After verifying the worker really stopped, run
`agent-recover ID RUN --confirmed-stopped`. This only repairs the local record,
it does not stop a live process. Demo mode never calls either CLI. Work data is
not synchronized with dotfiles or Linear.
