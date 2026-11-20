# Changelog

All notable changes to this project are documented in this file according to the
conventions in _[Keep a Changelog]_. Release versions use [semantic versioning].

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[semantic versioning]: https://semver.org/spec/v2.0.0.html

## [0.4.0]

### Enhancements

**Codex**

**IMPORTANT:** This is a preview release of Codex support.

- The `codex:` session source reads Codex CLI sessions: `gage session list`,
  `gage session view`, `gage session add`, `gage blame`, and `gage query` work
  on them as they do on Claude Code sessions
- `gage init` sets Gage up in every installed coding agent it has a driver for,
  Claude Code and Codex, and no longer fails when Claude Code is not installed.
  `--driver` names the drivers to set up, and `gage init --remove` removes Gage
  from the same agents
- The Gage plugin installs into Codex with its MCP tools and the `blame`,
  `publish`, and `resolve` skills
- Scanner agents run on Codex. A model with the `codex` scheme, as in
  `codex:small`, or a size when `codex` is the default driver, runs the agent on
  Codex over sessions of either driver. Codex reports no cost for an agent
- `gage resolve` and `gage publish` start their session in the default driver's
  coding agent, Claude Code or Codex
- An issue assigned from a Codex session records that session as its holder
- The `drivers.default` setting names the driver that a session source or an
  agent model with no scheme goes to. `gage init` writes it when the config
  names none, asks when more than one driver is set up, and replaces it with
  `--default`
- A scanner agent's model takes a driver scheme, as in `claude:small`, which
  names the driver that runs the agent. A scan can read one driver's sessions
  while its agents run on another
- `drivers.<driver>.models.<size>` settings name the model a driver uses for the
  `small`, `medium`, and `large` model sizes
- `gage status` shows the coding agent, login, and plugin of each driver. An
  agent that is not installed is shown as not installed and is a problem only
  for the default driver. A plugin of another version than the build's is a
  problem that `gage init` fixes

**Pi**

- The `pi:` session source reads Pi coding agent sessions:
  `gage session list`, `gage session view`, `gage session add`, and
  `gage query` work on them as they do on Claude Code sessions. Gage reads and
  stores Pi sessions; it is not set up in Pi and does not run agents on it

**Remotes**

- `gage fetch` fetches from a remote without changing the store's objects.
  `gage merge` merges what was fetched, and `gage pull` does both in one run
- `-R, --stored-remote <NAME>` reads the sessions fetched from a remote and not
  yet merged. `gage session list`, `view`, and `find`, `gage query`,
  `gage blame`, `gage index`, and `gage scan` accept it, as does the `query`
  tool through its `stored_remote` parameter
- The `query` tool's `gage_store` parameter reads the store at a path in place
  of the main store, as `GAGE_STORE` does for a command. With `stored_remote`,
  the remote is one of that store
- `gage scan --stored-remote` scans fetched sessions and writes its dataset,
  notes, and issues to the store. These objects are withheld until the sessions
  are merged: `gage push` refuses them and `gage backup` leaves them out
- `-N` is the short form of `--native` on every command that takes a session
  source

**Scans**

- Experimental `checks` scanner applies the `general` and `code-review` checks
  to each session in one agent call
- `gage scan list` and the scan view report a cost of "unknown" in place of
  `$0.00+` when no agent reported a cost

### Fixes

- The `issue_assign` tool is always listed. It was left out when the server
  started without `CLAUDE_CODE_SESSION_ID` set, and a call whose session cannot
  be named is now refused with a message that says so

### Other changes

- `gage fetch` no longer merges and no longer accepts a URL, and its `--ours`
  and `--theirs` options moved to `gage merge`. `gage pull` does what
  `gage fetch` did before
- `--native <SOURCE>` replaces `-s, --source` on every command that takes a
  session source. The `query` tool's `source` parameter is renamed `native` to
  match
- `gage query` no longer reads another store through a `gage:<path>` source.
  `--stored-remote` reads a remote's fetched objects instead
- `gage remote rename` is faster

## [0.3.1]

Pending release

### Enhancements

- `gage remote rename` renames a remote and keeps its settings and what it
  tracked
- `gage remote edit` changes a remote's URL (`--url`) and backup target mark
  (`--backup`, `--no-backup`)
- `gage remote edit` adds, removes, and clears default push and fetch objects
  (`--add-push`, `--remove-push`, `--no-push`, and the `fetch` equivalents)
- `gage blame` finds sessions that recorded a file under another path to the
  same repository: a moved repository, a session from another machine, or a
  published session

### Fixes

- `gage blame` attributes an edit that was committed after an unrelated commit
- `gage blame` attributes a shell write to a file the command names relative to
  a `cd`, through a variable it assigned, or by its bare name as a redirect
  target or a quoted string in an inline script
- Files with `[`, `?`, or `*` in their path are recorded as read or written

### Other changes

- `--since <SINCE>` replaces `--days` and `--today` on `gage scan`,
  `gage session add`, `gage dataset add`, and `gage publish`. It takes a
  duration such as `1h`, `30m`, or `7d`, as `gage session list --since` does.
- Support reading sessions from different stores via query cmd/tools.

## [0.3.0]

### Enhancements

**Publish**

- `gage publish` writes a shareable copy of a session with secrets, personal
  information, system details, and unfit language removed
- Filters select what is removed: `gitleaks`, `secret`, `pii`, `system`,
  `language`, and `harness`, chosen by name or by group (`public`, `trusted`,
  `reading`)
- A pending publish is reviewed before it is written: `gage publish view`,
  `show`, `edit`, `refresh`, and `apply`
- `gage publish --batch` publishes without an interactive session and writes a
  session again only when it changed
- User defined filters load from the project, `~/.gage/local/filters/`, and
  `~/.gage/lib/filters/`
- A `publish` skill drives a publish from a Claude Code session

**Shares**

- `gage share` collects published sessions into a set to push to others
- A share names the filters every member must have been published with
- `gage publish --share` adds a published session to a share

**Push and fetch**

- `gage remote edit` records the objects a remote pushes and fetches by default
- `gage push` and `gage fetch` use those defaults when no object is named

**Store**

- `GAGE_STORE` sets the store location
- A store opens inside an existing Git repository, where Gage writes only under
  `refs/gage/`

**Sessions**

- `gage session find` limits a search with `--since`, `--user`, `--assistant`,
  and `--tool`
- `gage session move` moves a native session to another project
- Listings are not truncated when output is piped

**Query**

- `tool_use_file` and `native_tool_use_file` list the files each tool call
  wrote, read, or cited
- `session_change` lists what a publish changed in a session
- A `help` table documents the query surface by topic
- `session` and `native_session` gain title, session type, and summary columns

**TUI**

- The session view shows attachments
- The session view shows redacted text for a published session
- Session entries render with a consistent layout for attributes and body
  content
- The native source of an entry is shown as JSON

### Fixes

- Token and cost totals include refused requests, subagent runs, and the title
  model
- Invalid JSON lines in a session file no longer fail the read
- Redraw loops in the scan view and app event handling
- Continue prompts when `--yes` is given
- Project filter in `gage session list`
- Git repository resolution with relative paths

### Other changes

- `query` and `tools` skills removed; the `help` table and the MCP server
  instructions replace them
- Release binary optimized for size
- DataFusion upgraded

## [0.2.0]

### Enhancements

**Sharing and backup**

- `gage push` and `gage fetch` move objects between a store and a remote, taking
  everything an object links
- `gage backup` copies the whole store to a remote, and `gage fetch` restores it
- `gage remote` manages remotes

**Datasets, attachments, and tags**

- `gage dataset` collects sessions and attachments into a set you scan as a unit
- `gage attachment` gives scanners files to read, such as a project's config and
  rule files
- `gage tag` names a stored object, and the name is accepted wherever an ID is

**Query**

- `gage query` reads the whole store with SQL: sessions, tool calls, notes,
  issues, scans, datasets, attachments, and tags
- `gage query` runs SQL from arguments or from a `.sql` script file
- `gage blame` finds the sessions that wrote given lines of a file

**Resolve**

- `gage resolve` reclaims issues left assigned by an interrupted run
- `gage resolve` selects its model with `--model`

**TUI**

- A store view browses stored objects as a tree
- The session view filters which entries appear and opens at a chosen line
- A scan view shows a scan's tasks, notes, issues, and live agent sessions

**Sessions**

- Session listings show token count and cost

### Other changes

- Store moved from a SQLite database at `~/.gage/data/gage.db` to a Git
  repository at `~/.gage/store.git`; 0.1.x data does not carry forward
- Config is a single file at `~/.gage/config.toml`; project-level
  `.gage/config.toml` is removed
- `gage pull` replaced by `gage fetch`
- `gage config show` replaced by `gage config list` and `gage config get`
- `gage mcp` serves stdio only; the `http` subcommand removed
- `gage agent` and `gage test` commands removed
- `finding-issues` scanner renamed to `findings`
- Scanners load from the project, `~/.gage/local/scanners/`, and
  `~/.gage/lib/scanners/`

## [0.1.1]

### Enhancements

- User defined scanners

### Fixes

- Agent auth errors for scans on macOS
- Scanner parse error handling

### Other changes

- Use plugin defined agents for resolve skill tasks
- Use 'wontfix' instead of 'skipped' as close reason
- Scanner Rune source cleanup

## [0.1.0]

This is the initial Gage release (MVP).

Refer to docs for features.

## [0.1.0-rc.4]

Misc fixes

## [0.1.0-rc.3]

- Release candidate for 0.1.0 (MVP)
