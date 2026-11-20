---
title: gage init
description:
  Command reference for gage init, which sets up Gage in the installed coding
  agents
---

The `gage init` command sets up Gage. It creates the store when there is none
and installs the Gage plugin, an MCP server and skills, in each coding agent
it can be set up in: Claude Code (`claude`) and Codex (`codex`). Run it again
after upgrading Gage so each agent gets the plugin of the new version. Gage
reads Pi sessions and is not set up in Pi.

```cli
gage init [OPTIONS]
```

| Option              | Description                    |
| ------------------- | ------------------------------ |
| `-d, --driver NAME` | Driver to set up (repeatable)  |
| `--default NAME`    | Default driver                 |
| `-r, --remove`      | Remove Gage from coding agents |
| `-y, --yes`         | Skip prompts                   |

Without `--driver`, Gage is set up in every installed agent it has a driver for.
With `--driver`, only the drivers named are set up. No installed agent is an
error.

For Codex the plugin's MCP server is registered with its tools approved. Codex
otherwise refuses a tool that changes anything when it runs without a person to
ask.

## Default driver

The default driver is where a source or a model with no driver named goes. It is
the `drivers.default` setting; see
[`gage config`](/docs/commands/config#settings).

`gage init` leaves a default the config file already names as it is, so running
it again asks nothing. When the file names none:

- One driver is set up: it becomes the default.
- Two or more are set up: `gage init` asks which is the default.
- Two or more are set up and `--yes` is given: `--default` is required.

`--default` names the default driver, and replaces one the config file already
names. The driver's agent must be installed. It need not be one of the drivers
this run sets up.

## Remove

`gage init --remove` removes the plugin from every installed agent, or from the
agents `--driver` names. The store and the config stay.

To remove the rest of a Gage installation, see
[_Uninstall Gage_](/docs/uninstall).
