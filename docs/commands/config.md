---
title: gage config
description:
  Command reference for gage config, including subcommands to show and edit Gage
  configuration
---

The `gage config` command manages Gage configuration. Config lives in one TOML
file, `~/.gage/config.toml`. When `GAGE_HOME` is set, the file is
`$GAGE_HOME/config.toml`.

```cli
gage config <COMMAND>
```

## config show

Show the effective Gage configuration.

```cli
gage config show
```

The output is TOML. Every setting is listed with its effective value. A setting
that is not present in the config file is marked `# default`.

```toml
# ~/.gage/config.toml

[issues]
related_threshold = 0.2  # default

[scanners]
disable = ["hidden-*"]
enable = []  # default

[storage]
max_session_size = "256MB"  # default
```

## config edit

Edit the Gage config file in the system editor.

```cli
gage config edit
```

The editor is `$VISUAL`, then `$EDITOR`, then `vi`. If the config file does not
exist, it is created with default values before the editor opens.

## Settings

| Setting                    | Default   | Description                                                                                                                                                     |
| -------------------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `scanners.enable`          | `[]`      | When non-empty, only these scanners run by default. Entries may contain `*` wildcards.                                                                          |
| `scanners.disable`         | `[]`      | Scanners that never run by default. Takes precedence over `enable`. Entries may contain `*` wildcards.                                                          |
| `issues.related_threshold` | `0.2`     | Similarity score at or above which `gage issue show` lists an issue as related.                                                                                 |
| `storage.max_session_size` | `"256MB"` | Largest total content a single session may write to the store. Accepts a byte count or a `KB`, `MB`, or `GB` suffix. `--max-size` overrides it for one command. |
