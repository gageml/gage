---
title: Uninstall Gage
---

Gage does not yet have an uninstaller.

To manually uninstall Gage run these steps.

##### Step 1. Remove the plugin from your coding agents

```cli
gage init --remove
```

##### Step 2. Delete Gage binary

```shell
rm ~/.local/bin/gage
```

##### Step 3 (optional). Delete Gage data

```shell
rm -r ~/.gage
```
