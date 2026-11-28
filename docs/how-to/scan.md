---
title: Scan a session
description:
  How to scan a session with Gage to find the problems in it and resolve them
---

A session holds problems that you did not see while you worked: an instruction
your agent ignored, a bug it wrote, a claim it did not check. A scan reads the
whole session and reports each problem as an issue that you can act on.

## Before you start

Install Gage and run `gage init`. See [Quick start](/start).

## Steps

1. Find the ID of the session.

   ```cli
   gage session list
   ```

2. Scan the session. Gage asks which scanners to run. Select the scanners
   you want and press Enter.

   ```cli
   gage scan 113509cd
   ```

3. List the issues the scan found.

   ```cli
   gage issue list
   ```

4. Resolve the issues. Gage starts your coding agent, which takes you through
   each issue and makes the fixes you approve.

   ```cli
   gage resolve
   ```
