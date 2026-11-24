---
title: gage config
description:
  Command reference for gage config, including subcommands to list, get, and
  edit Gage configuration
---

The `gage config` command manages Gage configuration. Config lives in one TOML
file, `~/.gage/config.toml`. When `GAGE_HOME` is set, the file is
`$GAGE_HOME/config.toml`. A project can set session sources in a file of its
own; see [Project config](#project-config).

```cli
gage config <COMMAND>
```

## config list

List the effective configuration.

```cli
gage config list
```

Every setting is printed as `name = value` with its working value: the value in
the config file, or the default when the file does not set it. A `native`
setting that a [project config](#project-config) sets is printed with the
project's value.

```toml
backup.native = []
drivers.claude.models.large = "claude-opus-4-8"
drivers.claude.models.medium = "claude-sonnet-4-6"
drivers.claude.models.small = "claude-haiku-4-5"
drivers.codex.models.large = "gpt-6.1-sol"
drivers.codex.models.medium = "gpt-6.1-terra"
drivers.codex.models.small = "gpt-6-luna"
drivers.default = "claude"
drivers.pi.models.large = "anthropic/claude-opus-4-8"
drivers.pi.models.medium = "anthropic/claude-sonnet-4-6"
drivers.pi.models.small = "anthropic/claude-haiku-4-5"
issues.related_threshold = 0.2
scanners.disable = []
scanners.enable = []
storage.max_session_size = "256MB"
```

## config get

Show the effective value of one setting.

```cli
gage config get <NAME>
```

`NAME` is dotted, as printed by `gage config list`. A string prints without
quotes. Any other value prints as it appears in the config file.

## config edit

Edit the Gage config file in the system editor.

```cli
gage config edit
```

The editor is `$VISUAL`, then `$EDITOR`. If the config file does not exist, it
is created with default values before the editor opens.

## Settings

| Setting                          | Default        | Description                                                                                                                                                                             |
| -------------------------------- | -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `scanners.enable`                | `[]`           | When non-empty, only these scanners run by default. Entries may contain `*` wildcards.                                                                                                  |
| `scanners.disable`               | `[]`           | Scanners that never run by default. Takes precedence over `enable`. Entries may contain `*` wildcards.                                                                                  |
| `issues.related_threshold`       | `0.2`          | Similarity score at or above which `gage issue show` lists an issue as related.                                                                                                         |
| `storage.max_session_size`       | `"256MB"`      | Largest total content a single session may write to the store. Accepts a byte count or a `KB`, `MB`, or `GB` suffix. `--max-size` overrides it for one command.                         |
| `backup.native`                  | `[]`           | Session sources whose sessions `gage backup` adds to the store before each backup. Each entry is `SCHEME:PATH`, a scheme alone, or a name set under `native`. `gage backup --native` replaces the list for one run. |
| `drivers.default`                | `"claude"`     | Driver that a scanner agent model with no scheme goes to, and driver whose default directory a command reads when it names no session source and `native.default` is not set. `gage init` writes it when the config file does not set it, and `gage init --default` replaces it. |
| `drivers.<driver>.models.<size>` | driver default | Model that a driver uses for a model size. `<size>` is `small`, `medium`, or `large`. Scanner agents and `gage resolve` and `gage publish` sessions that name a size run on this model. |
| `native.default`                 | not set        | Session source that a command reads when it names none. See [Default session source](#default-session-source).                                                                          |
| `native.<name>`                  | not set        | Session source that `<name>` stands for wherever a session source is given. See [Native source names](#native-source-names).                                                           |

A driver's model sizes are set in a `models` table under the driver's name:

```toml
[drivers.claude.models]
medium = "claude-sonnet-4-6"
large = "claude-opus-4-8"
```

A size the table does not set keeps the driver's default. `gage config list`
shows the model each size uses.

The models a Codex account can use depend on its ChatGPT plan, and Gage does not
check them. When the plan lacks a size's default model, an agent that names the
size fails with Codex's message. Set the size to a model the plan has:

```toml
[drivers.codex.models]
medium = "gpt-5.6-terra"
large = "gpt-5.6-terra"
```

Pi reaches many model providers and names a model as `<provider>/<id>`. The
sizes of the `pi` driver default to Anthropic models. A Pi user of another
provider sets each size to a model of that provider, named as
`pi --list-models` names it:

```toml
[drivers.pi.models]
small = "anthropic/claude-haiku-5-5"
medium = "anthropic/claude-sonnet-5-5"
large = "anthropic/claude-opus-5-5"
```

`gage status` and the check a scan makes before it starts ask Pi whether it is
signed in to the provider of the `medium` model.

### Native source names

The `native` table gives names to session sources. A name is accepted wherever
a session source is: the `--native` option of every command, the `backup.native`
setting, and the `native` parameter of the `query` tool.

```toml
[native]
work = "pi:~/work/agent"
```

```cli
gage session list --native work
```

[`gage native`](/commands/native) adds, lists, edits, renames, and deletes
the names without opening the file.

A source is `SCHEME:PATH` or a scheme alone. A name cannot stand for another
name. A name must not be empty and must not contain a colon.

A name is looked up before a scheme. A name that is also a scheme, such as
`claude`, replaces the scheme written alone. The scheme with its colon,
`claude:`, still names the default directory of the scheme.

A source in the config file has an absolute path or a path under `~`. A
relative path there is an error.

A command fails when the table holds a name or a source that breaks these rules.
`gage config` runs with such a table, so `gage config edit` can correct it.

### Default session source

The name `default` in the `native` table is the default session source: the
source a command reads when `--native` is omitted.

```toml
[native]
default = "pi:~/work/agent"
```

The value follows the rules of every other name in the table. It is
`SCHEME:PATH` or a scheme alone, and it cannot be another name. When no config
sets `native.default`, a command reads the default directory of the default
driver, which the `drivers.default` setting names.

`native.default` does not change the default driver. Scanner agents and
`gage resolve` and `gage publish` sessions run on the driver that
`drivers.default` names.

### Project config

A project sets session sources in `.gage/config.toml` under the project
directory. A command reads the file when it runs in the project directory or in
a directory below it.

```toml
[native]
default = "claude:sessions"
```

With this file in `~/code/app/.gage/config.toml`, `gage session list` run under
`~/code/app` lists the Claude Code sessions under `~/code/app/sessions/projects`.

A project config holds the `native` table and nothing else. A command fails
when the file sets any other table.

A name the project config sets replaces the same name in `~/.gage/config.toml`.
The other names of that file stay in effect. When more than one directory
between the current directory and the home directory has a project config, the
file nearest the current directory takes precedence. The home directory is not
a project.

A relative path in a project config is under the project directory, which is
the directory that holds `.gage`. An absolute path and a path under `~` are
read as written.

`gage config edit` edits `~/.gage/config.toml`. A project config is edited as a
file.
