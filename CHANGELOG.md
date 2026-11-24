# Changelog

All notable changes to this project are documented in this file according to the
conventions in _[Keep a Changelog]_. Release versions use [semantic versioning].

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[semantic versioning]: https://semver.org/spec/v2.0.0.html

## [unreleased]

### Enhancements

- A project config names a store of the project's own: `path` in a `store`
  table of `.gage/config.toml`, as a path under the project's `.gage` directory.
  Commands run in the project use that store. `GAGE_STORE` takes precedence
  over it
- `gage status` marks a store that `GAGE_STORE` or a project config selected

## [0.6.0]

### Breaking changes

- The `session_change` table is renamed `session_publish_change`. A query that
  names `session_change` fails
- A scanner narrows the query tool with methods: `Query::new().session(s)` for
  one session and `Query::new().session(s).range(start, end)` for a line range
  of it. `Query::with_session` and `Query::with_session_range` are removed
- A scanner gives a line range with `range(start, end)`, where it was
  `lines(start, end)`: on `messages()`, `tool_uses()`, and `compress()`

- A session source in the `native` table of `~/.gage/config.toml` with a
  relative path is an error. A name `default` in that table is now the default
  session source

### Enhancements

- `gage native` manages the names of session sources with `add`, `list`, `edit`,
  `rename`, and `delete`, where a name was set only by editing a config file.
  `gage native list` shows each name in effect in the current directory, its
  source, and the config file that sets it, with the built-in schemes.
  `gage native add --project` writes the project config file
- `gage merge` and `gage pull` take `--type` to merge every object of a type,
  such as `issue`, with everything those objects link
- The `native.default` setting names the default session source: the source a
  command reads when `--native` is omitted. When no config sets it, a command
  reads the default directory of the default driver, as before
- A project sets session sources in `.gage/config.toml` under the project
  directory. A command run in the project directory or below it reads the
  `native` table of that file, and a name the file sets replaces the same name
  in `~/.gage/config.toml`. A relative path in the file is under the project
  directory
- A scanner defines query views for its scan in an `init` function:
  `context().query_view(name, sql).description(text)`. A view is a named
  `SELECT` over the scan's tables. The query tool serves the views to every
  agent of the scan and lists them in its description. Gage calls `init` once
  for each scanner, with the scanner's params, before any task starts. A view
  that cannot be created stops the scan before any task

### Other changes

- `gage session list --stored` lists sessions by the time of their last entry,
  most recent first, and shows that time in an `Active` column. It ordered them
  by the store's last write and showed when each was added, in a `Created`
  column, before. `--share` is listed the same way. `--dataset` keeps member
  order and shows the `Active` column
- `gage scan delete` keeps a note or an issue that another scan run or an issue
  still uses, and reports the number it kept. It deleted every note and issue
  the scan run wrote before, including an issue that an earlier scan run also
  wrote and a note that a later scan run carried forward
- The confirmation prompt of `gage fetch` and `gage pull` selects "No" when it
  opens, where it selected "Yes" before
- `gage init` shows one line for each coding agent it sets up or removes, such
  as "Installing support for Claude Code", where it showed a line for each step
- `gage init` no longer shows a line for a store that already exists
- `gage backup` no longer reports the objects and tags that are only on the
  remote, and no longer suggests `--prune` for them. A remote that several
  stores back up to holds objects that each store lacks. `--prune` still lists
  what it deletes
- `gage backup` no longer reports an object as diverged when the remote holds a
  newer edit of it that the store also holds. It counts the object as newer on
  the remote, exits without an error, and does not replace it under `--force`,
  where `--force` replaced the newer edit with the older one before
- A change to an object keeps what a newer release of Gage added to it: files
  and attributes that this release does not recognize were removed by an edit or
  a merge before
- A change to an object keeps the object's type version, where it was raised to
  the version of the release that made the change before
- An issue with a file under `changes` that this release does not recognize is
  read, where it was an error before

## [0.5.0]

### Enhancements

**Pi**

- The `pi:` session source reads Pi coding agent sessions: `gage session list`,
  `gage session view`, `gage session add`, and `gage query` work on them as they
  do on Claude Code sessions
- `gage init` sets Gage up in Pi: it installs the Gage skills as a Pi package
  and registers the Gage MCP server. The skills are named `gage-blame`,
  `gage-publish`, and `gage-resolve` in Pi. `gage init --remove` removes both,
  and `gage status` shows Pi's version, its sign-in, and the install
- Pi can be the default driver. Scan agents run on Pi, and `gage resolve` and
  `gage publish` start Pi sessions
- The `drivers.pi.models` settings name the model of each size for Pi. They
  default to Anthropic models
- A `gage` command run from a Pi session's shell knows the session it runs in.
  The `issue_assign` tool does not work in Pi, which does not name the session
  to an MCP server

**Sessions**

- A session source can be a scheme alone, which is the default directory of that
  scheme: `--native claude` is `--native claude:`
- The `native` settings give names to session sources. With
  `work = "pi:~/work/agent"` under `[native]`, `--native work` reads that
  source. A name is accepted wherever a session source is, and is looked up
  before a scheme

**Issues**

- When the `issue_assign` tool cannot identify the session a call comes from,
  its error names the coding agent that made the call and its version

**Remotes**

- The `query` tool's `gage_store` parameter reads the store at a path in place
  of the main store, as `GAGE_STORE` does for a command. With `stored_remote`,
  the remote is one of that store

**Scans**

- `gage scan --ignore-missing-harness` starts a scan when the default driver's
  harness is not installed or not logged in. A scan whose tasks start no agents
  then runs without a harness

### Fixes

- A scanner tool handler that fails with a `gage::Error`, as `?` on a runtime
  function such as `write_note` does, gives the agent the error's message. The
  agent received "internal server error" before and could not correct its call
- A scan records the params its scanners ran with: the declared defaults with
  the configured and `#{...}` values applied. The `params` column of the
  `scan_task` table holds them as a JSON object. A scan made before this has
  null there
- `--since`, `--limit`, and `--sample` select stored sessions by the time of
  their last entry. They went by the time the store last wrote the session, so a
  rewrite of a session's metadata made an old session count as recent. This
  applies to `gage scan --stored` and `--stored-remote`, to `gage publish`, and
  to `--since` on `gage session list` and `gage session find` over stored
  sessions. A session with no dated entry still goes by the store's time
- The `query` tool says how to read the next page of a result: a page that has
  another ends with the arguments of the call that reads it, and the parameter
  descriptions say that `cursor` and `page` are sent without `sql`. Agents sent
  all three, which the tool refuses, and the refusal now says what to send

### Other changes

- The `backup.sources` setting is renamed `backup.native`. A config file that
  sets `backup.sources` must be edited, because the old name is not read
- A session source must name its driver. A path with no scheme and a scheme that
  no driver serves are errors, where both went to the default driver before. A
  `backup.native` entry that is a path alone must be edited to add its scheme.
  The `drivers.default` setting names the driver whose default directory a
  command reads when it names no session source

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

**Remotes**

- `gage fetch` fetches from a remote without changing the store's objects.
  `gage merge` merges what was fetched, and `gage pull` does both in one run
- `-R, --stored-remote <NAME>` reads the sessions fetched from a remote and not
  yet merged. `gage session list`, `view`, and `find`, `gage query`,
  `gage blame`, `gage index`, and `gage scan` accept it, as does the `query`
  tool through its `stored_remote` parameter
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
