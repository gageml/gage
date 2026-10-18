# Changelog

All notable changes to this project are documented in this file according to the
conventions in _[Keep a Changelog]_. Release versions use [semantic versioning].

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[semantic versioning]: https://semver.org/spec/v2.0.0.html

## [0.2.0-rc.1]

### Enhancements

**Sharing and backup**

- `gage push` and `gage fetch` move objects between a store and a remote,
  taking everything an object links
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
