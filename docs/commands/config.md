---
title: gage config
description:
  Command reference for gage config, including subcommands to show and edit Gage
  configuration
---

The `gage config` command manages Gage configuration. Config lives in TOML
files: user config at `~/.gage/config.toml` and project config at
`.gage/config.toml` in a project directory.

```cli
gage config <COMMAND>
```

## config show

Show the effective Gage configuration.

```cli
gage config show
```

## config edit

Edit a Gage config file in the system editor.

```cli
gage config edit [OPTIONS]
```

| Option         | Description                                   |
| -------------- | --------------------------------------------- |
| `--user`       | Edit the user config at `~/.gage/config.toml` |
| `--project`    | Edit the nearest project `.gage/config.toml`  |
| `--path <DIR>` | Edit `<DIR>/.gage/config.toml`                |
