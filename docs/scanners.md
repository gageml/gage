---
title: Scanners
description:
  What a scanner is, where Gage loads scanners from, and how scanners are
  grouped
---

A scanner is a small program that reports issues it finds in your sessions or
project config.

Scanners are written in the
[Rune programming language](https://rune-rs.github.io/). They're run in a
sandbox virtual machine to limit what they can do on your system.

## Install scanners

Gage does not include scanners. You install the ones you want. `gage init`
offers to install the scanners that Gage publishes.

Scanners are published in the
[gage-scanners](https://github.com/gageml/gage-scanners) repository, where
their source code is available for review at any time. To see what you can
install, run:

```cli
gage scanner search
```

To install a scanner, run:

```cli
gage scanner install <NAME>
```

`gage scanner upgrade` installs newer versions of the scanners you have. See
[`gage scanner`](/commands/scanner).

You can inspect installed scanners on your system in `~/.gage/lib/scanners/`.
That directory belongs to the scanner commands. To change a scanner, copy it
to one of the other locations below and edit the copy.

## Scanner locations

Gage loads scanners from three places. A scanner in the first place shadows
one with the same name in the second.

1. `.gage/scanners/` in the current project directory or any directory above it
2. `~/.gage/local/scanners/`, for scanners you write for yourself
3. `~/.gage/lib/scanners/`, the scanners you installed

A scanner in the first two places is a directory holding a `scanner.rn` file,
or a single `.rn` file. An installed scanner is the directory
`NAMESPACE/NAME` under the third place, and that is also its full name. A
scanner in the first two places has the full name `local/NAME`. A name without a
namespace selects the local scanner before the installed one, and the full name
selects either. `gage scanner list` shows every scanner
Gage finds and where it came from. To run a scanner that is not in one of
these places, pass its path to `gage scan -s`.

## Select scanners for a scan

No scanner runs unless you select it. `gage scan -s NAME` runs a scanner by
name. `gage scan` with no scanner selection asks which scanners to run.

A scanner can declare the groups it belongs to. `gage scan -g NAME` runs every
scanner in a group.
