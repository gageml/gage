---
title: gage config
description:
  Command reference for gage config, including subcommands to list, get, and
  edit Gage configuration
---

The `gage config` command manages Gage configuration. Config lives in one TOML
file, `~/.gage/config.toml`. When `GAGE_HOME` is set, the file is
`$GAGE_HOME/config.toml`.

```cli
gage config <COMMAND>
```

## config list

List the effective configuration.

```cli
gage config list
```

Every setting is printed as `name = value` with its working value: the value in
the config file, or the default when the file does not set it.

```toml
backup.native = []
drivers.claude.models.large = "claude-opus-4-8"
drivers.claude.models.medium = "claude-sonnet-4-6"
drivers.claude.models.small = "claude-haiku-4-5"
drivers.codex.models.large = "gpt-6.1-sol"
drivers.codex.models.medium = "gpt-6.1-terra"
drivers.codex.models.small = "gpt-6-luna"
drivers.default = "claude"
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
| `backup.native`                  | `[]`           | Session sources whose sessions `gage backup` adds to the store before each backup. Each entry is `SCHEME:PATH` or a scheme alone. `gage backup --native` replaces the list for one run. |
| `drivers.default`                | `"claude"`     | Driver whose default directory a command reads when it names no session source, and driver that a scanner agent model with no scheme goes to. `gage init` writes it when the config file does not set it, and `gage init --default` replaces it. |
| `drivers.<driver>.models.<size>` | driver default | Model that a driver uses for a model size. `<size>` is `small`, `medium`, or `large`. Scanner agents and `gage resolve` and `gage publish` sessions that name a size run on this model. |

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
