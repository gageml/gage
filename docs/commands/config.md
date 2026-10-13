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

Every setting is printed as `name = value` with its working value: the value
in the config file, or the default when the file does not set it.

```toml
backup.sources = []
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

| Setting                    | Default   | Description                                                                                                                                                     |
| -------------------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `scanners.enable`          | `[]`      | When non-empty, only these scanners run by default. Entries may contain `*` wildcards.                                                                          |
| `scanners.disable`         | `[]`      | Scanners that never run by default. Takes precedence over `enable`. Entries may contain `*` wildcards.                                                          |
| `issues.related_threshold` | `0.2`     | Similarity score at or above which `gage issue show` lists an issue as related.                                                                                 |
| `storage.max_session_size` | `"256MB"` | Largest total content a single session may write to the store. Accepts a byte count or a `KB`, `MB`, or `GB` suffix. `--max-size` overrides it for one command. |
| `backup.sources`           | `[]`      | Session sources whose sessions `gage backup` adds to the store before each backup. `gage backup --source` replaces the list for one run.                        |
