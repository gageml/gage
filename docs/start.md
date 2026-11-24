---
title: Quick start
description:
  How to install Gage, set it up in your coding agent, and run a first scan
---

:::note

Gage is in early and active development. To provide feedback or PRs, visit our
[GitHub project](https://github.com/gageml/gage).

:::

Install Gage:

:::tab[Linux/macOS]{group=platform}

```shell
curl -fsSL https://gage.io/install.sh | sh
```

:::

:::tab[Windows]

Windows support planned ---
[upvote here](https://github.com/gageml/gage/issues/14)

:::

Set up Gage in your coding agent. Gage supports Claude Code, Codex, and Pi, and
sets itself up in each one that is installed:

```cli
gage init
```

Run a scan:

```cli
gage scan
```

List open issues:

```cli
gage issue list
```

Resolve issues:

```cli
gage resolve
```

For steps to uninstall, see [_Uninstall Gage_](/uninstall).
