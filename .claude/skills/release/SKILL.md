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

Identify the jj change ID to build and set the `build` bookmark to it:

```shell
jj bookmark set build -r <CHANGE>
jj git push -b build
```

Wait for the Build workflow to finish on every target. This can take as quite
some time. Monitor the build at https://github.com/gageml/gage/actions.

If the build fails, resolve the issues and re-run the commands above to
reference the new change. Move to the next section when the build succeeds.

## Release

Point the `release` bookmark at the change and push it:

```shell
jj bookmark set release -r build
jj git push -b release
```

The Release workflow creates the GitHub release named `VERSION`, attaches the
binaries and checksums, and creates the Git tag. A version with a suffix is
marked as a prerelease.

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

3. Build the release:

```shell
jj b set build -r @
jj git push -b build
```

The build process takes time. To resumed local work merge the release line into
the working dev line.

4. Merge the release line

```shell
jj new <PARENT> @ -m "Merge <VERSION>"
```

`<PARENT>` is the change ID of the working line.

This creates a commit using `<PARENT>` as first parent and `@` (the release
line) as the second parent.

5. When the build finishes, generate the release

```shell
jj b set release -r build
jj git push -b release
```

Wait for the release (typically runs in under a minute) and fetch the release
tag.

```shell
jj git fetch
```
