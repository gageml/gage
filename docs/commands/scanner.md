---
title: gage scanner
description:
  Command reference for gage scanner, including subcommands to list, search,
  show, install, uninstall, and upgrade scanners
---

The `gage scanner` command manages scanners. For background, see
[Scanners](/scanners).

```cli
gage scanner <COMMAND>
```

Scanners are installed from the scanner source, a Git repository. The default
is [gage-scanners](https://github.com/gageml/gage-scanners). The
`scanners.source` setting names another repository, as a URL or as the path of
a local repository. See [`gage config`](/commands/config#settings).

The source holds each scanner in a directory named `NAMESPACE/NAME`. The
`gage` namespace holds the scanners that Gage publishes. A scanner is installed
to `~/.gage/lib/scanners/NAMESPACE/NAME`.

A scanner's full name is `NAMESPACE/NAME`, as in `gage/general`. A scanner that
was not installed is in the `local` namespace, as in `local/mine`. These are the
scanners of a project, the scanners under `~/.gage/local/scanners/`, and a
scanner given to `gage scan` as a path. The source cannot hold a `local`
namespace.

A `SCANNER` argument is a full name or the name alone, such as `general`. The
name alone selects the local scanner of that name, and otherwise the installed
one. When two installed scanners have the name, give the full name.

Gage reads the source with Git when you run `gage scanner install`,
`gage scanner upgrade`, `gage scanner search`, `gage scanner list --outdated`,
or `gage scanner show` for a scanner that is not installed. No other command
reads it.

## scanner list

List the scanners Gage finds: installed scanners, your own scanners, and
project scanners.

```cli
gage scanner list [OPTIONS]
```

| Option           | Description                                       |
| ---------------- | ------------------------------------------------- |
| `-o, --outdated` | Show installed scanners that have a newer version |

The `Source` column says where a scanner is. It is `installed`, `user`, or
`project`. A scanner that the config disables is marked `(disabled)`.

`--outdated` shows each installed scanner with its installed version and the
newer version in the source. A note says when the newer version needs a newer
Gage.

## scanner search

Search the scanner source for scanners to install.

```cli
gage scanner search [TERM]...
```

The command shows the scanners whose name or description has every `TERM`.
Letter case does not matter. Without a `TERM`, it shows every scanner in the
source.

The `Installed` column holds the version that is installed. It is empty for a
scanner that is not installed.

## scanner show

Show a scanner: its version, where it is, what it requires, and the tasks,
notes, and issues it declares.

```cli
gage scanner show <SCANNER>
```

A scanner that is not installed is read from the scanner source.

## scanner install

Install scanners from the scanner source.

```cli
gage scanner install [OPTIONS] <SCANNER>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

The command installs the latest version of each scanner. It also installs
every scanner that one of them requires. An installed scanner is replaced when
the source holds another version of it.

A scanner is not installed when it needs a newer Gage, or when a scanner of
the same name from another namespace is installed.

## scanner uninstall

Uninstall scanners.

```cli
gage scanner uninstall [OPTIONS] <SCANNER>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

The notes and issues that a scanner wrote stay in the store. A scanner that
another installed scanner requires is not uninstalled unless that scanner is
uninstalled with it.

## scanner upgrade

Upgrade installed scanners.

```cli
gage scanner upgrade [OPTIONS] [SCANNER]...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

Without a `SCANNER` argument, the command upgrades every installed scanner. A
scanner is upgraded when the source holds a newer version of it. The command
also installs every scanner that a newer version requires.

A scanner whose newer version needs a newer Gage is left as it is, and the
command says so.

## Versions and the scanner API

A scanner in the source declares a `version` and an `api` in its `SCANNER`
constant.

```rust (rune)
pub const SCANNER = #{
    name: "general",
    description: "General findings",
    version: "0.1.0",
    api: 1,
    // ...
};
```

`version` is a semantic version. Every change to a published scanner raises
it.

`api` is the scanner API level that the scanner was written for. Each Gage
release supports a range of levels. A scanner above the range needs a newer
Gage. A scanner below the range needs a newer version of itself, which
`gage scanner upgrade` installs. `gage scan` makes the same check before it
runs a scanner.

A scanner names the scanners it needs with `requires`. Each entry is
`NAMESPACE/NAME`.

```rust (rune)
pub const SCANNER = #{
    name: "code-review",
    requires: ["gage/project-summary"],
    // ...
};
```

A scanner of your own, under `~/.gage/local/scanners/` or `.gage/scanners/`,
does not have to declare any of these fields.
