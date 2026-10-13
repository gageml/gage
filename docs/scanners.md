---
title: Scanners
---

A scanner is a small program that reports issues it finds in your sessions or
project config.

See [Scanners](/scanners/) for a list of available scanners.

Scanners are written in the
[Rune programming language](https://rune-rs.github.io/). They're run in a
sandbox virtual machine to limit what they can do on your system.

[Scanner source code](https://github.com/gageml/gage/tree/main/scanners) is
available for review at any time.

You can inspect installed scanners on your system in `~/.gage/lib/scanners/`.

## Scanner locations

Gage loads scanners from three places. A scanner in a more specific place
shadows one with the same name in a less specific place.

1. `.gage/scanners/` in the current project directory or any directory above it
2. `~/.gage/local/scanners/`, for scanners you write for yourself
3. `~/.gage/lib/scanners/`, the scanners that ship with Gage

A scanner is a directory holding a `scanner.rn` file, or a single `.rn` file.
`gage scan --list-scanners` shows every scanner Gage finds and where it came
from. To run a scanner that is not installed, pass its path to `gage scan -s`.

## Groups

A scanner declares the groups it belongs to. `gage scan` with no scanner
selection runs the `default` group. `gage scan -g NAME` runs a group by name.
