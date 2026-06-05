# Gage

Gage is a tool to analyze Claude Code transcripts to find issues and
help resolve them.

To scan your sessions for issue, run:

```shell
gage scan
```

## Install

Supported plateforms:

- Linux only (macOS and Windows planned)

### Install prebuilt binary

To run the install script, use:

```shell
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/gageml/gage/releases/latest/download/gage-cli-installer.sh | sh
```

This installs the `gage` binary for your system to `~/.local/bin/gage`.
It does not require elevated privileges.

Alternatively, use `cargo-binstall`:

```shell
cargo binstall --git https://github.com/gageml/gage gage-cli
```

### Install from source

Gage requires the Rust toolchain to compile.

Follow the instructions at https://rustup.rs/ to install it.

Clone the Gage repo:

```shell
git clone https://github.com/gageml/gage.git
```

Install the Gage CLI:

```shell
cargo install --path gage-cli
```

Install the Gage Claude plugin:

```shell
gage init
```

## Features

### Scan Claude Code transcripts (session)

`gage scan` runs _scanners_ on your Claude Code sessions. Scanners are
self contained programs written in the
[Rune](https://rune-rs.github.io/) programming language. You can examine
their source code under [scanners](/scanners) in this repository.

When you run `gage scan` you can select the scanners you want to run.
Alternatively, you can specify each scanner using the `-s/--scanner`
option. By default, Gage runs all available scanners.

You can also specify how many sessions you want to scan. By default Gage
scans the last 20 sessions. To scan all available sessions, use the
`-a/--all` option. Otherwise you can set the number with `-l/--limit`.

Scanners look at Claude session files (under `~/.claude/projects/`) as
well as Claude configuration for scanned sessions. Scanners report
_evidence_ by writing notes and open _issues_ when there's enough
evidence.

Issues call your attention to something. Issues are meant to be resolved
by either completing them or by skipping them.

List unresolved issues:

```shell
gage issue list
```

### Resolve issues

Gage is designed to use Claude Code to evaluate and resolve issues. To
resolve issues, run the `/gage:review` command in Claude Code. This
command work in any standard Claude Code interface (e.g. CLI, VS Code
extension, etc.)

```
> /gage:review
```

Claude uses the available Gage tools to list and read open issues.
You're free to work through issues with Claude's help as you see fit.
Each issue provides information to confirm the problem and advice on
fixing it. If you decide it's not a problem, skip it --- Claude can
close the issue as `skipped`. If you resolve the issue, Claude can close
the issue as `completed`.

To review open issues, run:

```shell
gage issue list
```

View issue details:

```shell
gage issue show <ISSUE ID>
```

Close an issue:

```shell
gage issue close <ISSUE ID> [--skipped]
```

Delete an issue:

```shell
gage issue delete <ISSUE ID>
```

## Notes

_Notes_ are values associated to sessions, session lines, and project
config. Scanners write notes as evidence for issues.

List notes:

```shell
gage note list
```

Some scanners require a certain amount of evidence before opening an
issue. For this reason, notes are generally retained over time. Notes
are useful as factual records both for issues (evidence) and for session
analysis.

## Gage Query

`gage query` provides a SQL interface to all Gage data. This includes:

- Session data (e.g. line entry JSON)
- Notes
- Issues
- Project config

Scanners use this facility exclusively for read-only data.

Use `gage query -c SQL` to run queries yourself. This is useful for
analysis you'd like to perform on sessions, notes, or issues.

Gage Query provides a PostgreSQL compatible interface. The REPL supports
commands using the syntax `\COMMAND`. Run `\?` from the REPL to list
availble commands.

Note that Gage Query reads some data from local files (e.g. sessions and
project config). Some queries will cause full file system scans, which
are surprisingly slow and memory intensive. In general, avoid running
unbounded queries

Avoid:

```sql
SELECT * FROM entry;
```

This will read every sesion line into memory.

Instead, use `WHERE` and `LIMIT` clauses:

```sql
SELECT * FROM entry WHERE session_id LIKE 'abc123%' LIMIT 10`;
```

## FAQ

### What license is Gage available under?

[Apache 2](LICENSE.txt)

### Where is Gage data stored?

Gage writes all of its data to files under `~/.gage/`. These include:

- Settings (`settings.json`)
- Installed scanners (`lib/scanners/`)
- Data incuding notes and issues (`data/gage.db`)

### Does Gage "phone home" for any reason?

No. Gage runs locally and writes notes and all data under
`~/.gage/data/`.

Gage provides tools to Claude Code over a local MCP server. Claude is
free to use these tools within the constraints of user-defined
premissions (allow and deny). Gage tools do not open network connections
or otherwise write outside of `~/.gage/`. Claude, however, may. This is
the normal risk profile of running an agent. To minimize the risk of
sensitive data exfiltration, follow the safeguards recommended by
Anthropic.
