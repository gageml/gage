# install.sh guidelines

install.sh is an intentionally simplified, hand-rolled installer. The project
previously used cargo-dist and dropped it (commit 18bfafd); we own this script
outright. It supports exactly the platforms we publish release assets for and
nothing more.

Rules:

- POSIX sh only; dependencies limited to curl, tar, and sha256sum or shasum
- `detect_target` maps only platforms with published release assets;
  everything else fails with a clear "unsupported platform" error
- Checksum verification is mandatory - never skipped, never a warning
- No pipelines where the first command's failure matters. POSIX sh has no
  `pipefail` (dash and other /bin/sh implementations reject `set -o
  pipefail`), so a pipeline's status is the last command's and an upstream
  failure is masked. Capture the output in a variable with `|| err ...`,
  then post-process. Same for command substitution inside `[ ]` - its exit
  status is discarded there; assign to a variable first so `set -e` sees
  the failure.
- When a new build target is added to .github/workflows/build.yml, add the
  corresponding mapping to `detect_target`

Upstream reference: we spot-check cargo-dist's generated shell installer for
techniques worth borrowing (the Rosetta 2 detection via sysctl in
`detect_target` came from there):

https://github.com/axodotdev/cargo-dist/blob/main/cargo-dist/templates/installer/installer.sh.j2
