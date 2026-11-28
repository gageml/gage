# Privacy

Gage does not phone home.

- Gage has no telemetry, no analytics, no crash reporting, and no update check.
- Gage sends no logs, metrics, or usage data to any server, ours or anyone
  else's.
- Gage writes all of its data, including its logs, to files under `~/.gage/` on
  your machine.

## When data leaves your machine

Data leaves your machine in two cases only.

**You run a command that sends it.** `gage push` and `gage backup` send data to
a git remote that you configured. Gage sends data only to that remote and only
when you run the command. Gage shows what it will send and prompts you before it
sends anything. The default answer is "No". The `--yes` option skips the prompt.

**You run the scan command.** Some scanners use your coding agent, such as
Claude Code, Codex, or Pi. The agent sends session content to its model provider
under your account, as it does in any other session.

## Gage tools and your agent

Gage provides tools to your coding agent over an MCP server that listens on
`127.0.0.1` only. Gage tools do not open network connections and do not write
outside of `~/.gage/`.

Your agent can open network connections of its own. This is the normal risk of
running an agent. Use your agent's permission settings to control what it can
reach.

## Installation

The install script downloads a release of Gage from GitHub. It sends no
information about you or your machine beyond what any download request carries.

## Scanners

Gage downloads scanners with a Git fetch from the scanner source. The default
source is the `gageml/gage-scanners` repository on GitHub, and the
`scanners.source` setting names another. The fetch sends no information about
you or your machine beyond what any Git fetch carries.

Gage fetches from the scanner source only when you run one of these commands:

- `gage scanner install` and `gage scanner upgrade`
- `gage scanner search` and `gage scanner list --outdated`
- `gage scanner show` for a scanner that is not installed
- `gage init`, when no scanner is installed and you agree to install scanners or
  pass `--yes`

No other command reads the scanner source, and Gage never checks it without
being asked.

## Verify these claims

Gage is open source. Every claim in this file can be checked against the code in
this repository.
