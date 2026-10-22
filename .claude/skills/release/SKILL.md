---
name: release
description: Build and release Gage.
---

# Releasing Gage

A release is a GitHub release with built binaries attached. Two GitHub action
workflows are used:

- `build` runs on a push to the `build` branch and uploads one binary artifact
  per target.

- `release` runs on a push to the `release` branch, collects those artifacts,
  and creates the GitHub release.

The Release workflow creates the Git tag.

The Gage version comes from the `version` field in the root `Cargo.toml`. The
release notes come from the `## [VERSION]` section in `CHANGELOG.md`. Both must
be set before the Build push.

Do not run the commands below. Present them to the user and let the user run
them. The user does all pushes.

## Select the change

The change to release is any commit whose root `Cargo.toml` has the version
being released and whose `CHANGELOG.md` has a `## [VERSION]` section for it.
Note its change ID from `jj log`.

## Build

Point the `build` bookmark at the change and push it:

```shell
jj bookmark set build -r <change-id>
jj git push -b build
```

Wait for the Build workflow to finish on every target. The Release workflow
fails if any target artifact for this version is missing.

## Release

Point the `release` bookmark at the change and push it:

```shell
jj bookmark set release -r <change-id>
jj git push -b release
```

The Release workflow creates the GitHub release named `VERSION`, attaches the
binaries and checksums, and creates the Git tag. A version with a suffix is
marked as a prerelease.

## Afterward

Fetch the new tag:

```shell
jj git fetch
```

## Promote a release candidate

A final release is built from the release candidate commit with only the version
changed. The candidate code is not otherwise modified.

1. Create a new change on top of the release candidate tag:

```shell
jj new <rc-tag> -m "Release <VERSION>"
```

E.g. `jj new 0.2.0-rc.1 -m "Release 0.2.0"`

2. Modify the project to remove the `-rc.N` suffix from the version:

- The `version` value in the root `Cargo.toml`
- Section `CHANGELOG.md` header

3. Follow the Build and Release sections above.

Build the release:

```shell
jj b set build -r @
jj git push
```

If the build succeeds, generate the release:

```shell
jj b set release -r @
jj git push
```

4. Return to the development head:

```shell
jj new <development-head>
```
