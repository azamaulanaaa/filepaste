# Add the checks and tools to an existing repository

## Purpose and audience

This guide adds the checks and tools to an existing repository. You keep your
layout and your history. The guide copies files and merges settings. It
does not replace your files without a review.

Read this guide when you maintain an existing repository and want the
checks, the hooks, the CI jobs, and the release flow.

## What you get

| Feature | Entry point | What it does |
| --- | --- | --- |
| Checks | `scripts/ci.sh` | Runs every check. |
| Formatters | `scripts/format.sh` | Applies every formatter. |
| Git hooks | `.githook/` | Checks the message and runs the checks. |
| CI checks | `.github/workflows/ci.yml` | Runs the same checks on GitHub. |
| Release | `cog.toml`, `release.yml` | Creates the changelog and a tag. |
| Sandbox | `scripts/smolvm*.sh`, `smol/` | Runs the checks in a microVM. |
| Rust | `Cargo.toml`, `deny.toml` | Runs the Rust checks. |

## Before you start

Work on a branch:

```bash
git switch -c add-checks
```

Start from a clean tree. This command must print no lines:

```bash
git status --short
```

Save a copy of each file that you will merge:

```bash
cp -p Makefile Makefile.before-adopt
cp -p mise.toml mise.toml.before-adopt
cp -p .gitignore .gitignore.before-adopt
cp -p .editorconfig .editorconfig.before-adopt
cp -p cog.toml cog.toml.before-adopt
cp -p .github/workflows/ci.yml .github/workflows/ci.yml.before-adopt
```

These commands fail when a file is absent. Skip the missing file. A
missing file is not a problem.

You need these items:

| Requirement | Need | Notes |
| --- | --- | --- |
| `git` | required | The hooks and the checks use it. |
| `make` | required | The targets call the scripts. |
| `mise` | optional | Installs the tools from `mise.toml`. |
| `smolvm` and an accelerator | optional | Sandbox only. See Phase 7. |
| The check tools | required | The tools for the checks you select. |

The selected checks need `shellcheck`, `shfmt`, `cog`,
`markdownlint-cli2`, `editorconfig-checker`, `gitleaks`,
`jactionlint`, `zizmor`, `hadolint`, `dockerfmt`, and `dclint` on
`PATH`. The `mise` check also needs `mise`.

If you use `mise`, run `mise install`. Then activate `mise` in your
shell so the tools are on `PATH`.

### Platform

The check scripts need bash 4 or later because they use `mapfile`.
The gitleaks check uses GNU `cp --parents`. On macOS, provide GNU
coreutils on `PATH`, or adapt the copy step. This repository tests the
check pipeline on Linux. This repository does not test the smolvm macOS
path.

## Adoption phases

Each phase is independent. Stop after any phase and the repository
still works.

### Phase 1: Copy the check pipeline

Clone this repository to a staging directory:

```bash
git clone https://github.com/azamaulanaaa/init.git /tmp/template
```

Run these commands from your repository root:

```bash
mkdir -p scripts
cp -p /tmp/template/scripts/ci.sh scripts/ci.sh
cp -p /tmp/template/scripts/format.sh scripts/format.sh
cp -p /tmp/template/scripts/check-bash.sh scripts/check-bash.sh
cp -p /tmp/template/scripts/check-markdown.sh scripts/check-markdown.sh
cp -p /tmp/template/scripts/format-shfmt.sh scripts/format-shfmt.sh
```

These five files are examples. Copy `ci.sh` for the pipeline. Copy
`format.sh` for the formatters. Replace the two check names and the
format name with your selection from Phase 3.

`cp -p` keeps the file mode. A copied script stays executable. Check
the two entry points:

```bash
test -x scripts/ci.sh
test -x scripts/format.sh
```

`scripts/ci.sh` runs every `scripts/check-*.sh` in name order. It prints
`run: <script>` before each check. It stops when a check fails.

`scripts/format.sh` runs every `scripts/format-*.sh` in name order. It
stops when a formatter fails.

`scripts/ci.sh` can call `scripts/smolvm.sh`. It does this when
`SMOLVM_ENABLED` is not `0` and either `SMOLVM_ADDR` has a value or
smolvm and the accelerator are available. Phase 1 does not copy
`scripts/smolvm.sh`. A host with smolvm and a working accelerator can
therefore fail at `make ci` when you skip Phase 7. `SMOLVM_ADDR`
triggers the same handoff. Choose one option:

- Copy `scripts/smolvm.sh` and its helper files from Phase 7.
- Delete the smolvm handoff from `scripts/ci.sh`.
- Run every check with `SMOLVM_ENABLED=0`. For example:

  ```bash
  SMOLVM_ENABLED=0 make ci
  ```

Take one option and the phase still works on its own.

### Phase 2: Merge the Makefile

Do not overwrite an existing Makefile. Open the staged
`/tmp/template/Makefile` and merge it.

Add the header first:

```make
SHELL := bash
.DEFAULT_GOAL := help
```

`SHELL := bash` makes each recipe use bash. Without it, `make` uses
`/bin/sh`.

Add the `.PHONY` list from the staged Makefile:

```make
.PHONY: help ci smolvm smolvm-pack smolvm-clean smolvm-exec \
  smolvm-preflight smolvm-verify check-jactionlint check-bash \
  check-commits check-dclint check-dockerfmt check-editorconfig \
  check-gitleaks check-hadolint check-markdown check-mise \
  check-shfmt check-zizmor format format-shfmt format-dockerfmt \
  format-dclint hooks release
```

The committed file has this list on one line. A line continuation also
works.

Add your existing target names to the list, or add a second `.PHONY`
line. A target that is not in the list still runs. The list only stops
a name clash with a file.

Add the targets:

| Target | Recipe |
| --- | --- |
| `help` | Prints every target that has a `##` comment. |
| `ci` | `scripts/ci.sh` |
| `check-jactionlint` | `scripts/check-jactionlint.sh` |
| `check-bash` | `scripts/check-bash.sh` |
| `check-commits` | `scripts/check-commits.sh` |
| `check-dclint` | `scripts/check-dclint.sh` |
| `check-dockerfmt` | `scripts/check-dockerfmt.sh` |
| `check-editorconfig` | `scripts/check-editorconfig.sh` |
| `check-gitleaks` | `scripts/check-gitleaks.sh` |
| `check-hadolint` | `scripts/check-hadolint.sh` |
| `check-markdown` | `scripts/check-markdown.sh` |
| `check-mise` | `scripts/check-mise.sh` |
| `check-shfmt` | `scripts/check-shfmt.sh` |
| `check-zizmor` | `scripts/check-zizmor.sh` |
| `format` | `scripts/format.sh` |
| `format-shfmt` | `scripts/format-shfmt.sh` |
| `format-dockerfmt` | `scripts/format-dockerfmt.sh` |
| `format-dclint` | `scripts/format-dclint.sh` |
| `hooks` | Sets the hook path and the file modes. |
| `release` | `scripts/release.sh $(ARGS)` |
| `smolvm` | `scripts/smolvm.sh` |
| `smolvm-pack` | `smol/pack-build.sh` |
| `smolvm-clean` | `scripts/smolvm-clean.sh $(ARGS)` |
| `smolvm-exec` | `@scripts/smolvm-exec.sh "$(ARGS)"` |
| `smolvm-preflight` | `smol/preflight.sh` |
| `smolvm-verify` | `smol/verify.sh` |

Delete the targets for the checks and the sandbox files that you do not
copy.

The `help` target reads the `##` comment on each target. The staged
Makefile has the exact recipe. A recipe line starts with a tab. A line
with spaces fails.

The `help` target may need a merge if the repository already has one.
Keep one help target and one default goal.

### Phase 3: Select the checks

| Check | Tool | When to keep it |
| --- | --- | --- |
| `bash` | `shellcheck` | Keep for bash or sh files. |
| `shfmt` | `shfmt` | Keep for bash formatting. |
| `commits` | `cog` | Keep for Conventional Commits. |
| `markdown` | `markdownlint-cli2` | Keep for Markdown files. |
| `editorconfig` | `editorconfig-checker` | Keep for an `.editorconfig`. |
| `gitleaks` | `gitleaks` | Keep in most repositories. |
| `jactionlint` | `jactionlint` | Keep for workflow files. |
| `zizmor` | `zizmor` | Keep with `jactionlint`. |
| `hadolint` | `hadolint` | Keep for Dockerfiles. |
| `dockerfmt` | `dockerfmt` | Keep with `hadolint`. |
| `dclint` | `dclint` | Keep for Compose files. |
| `mise` | `mise` | Keep for `mise.toml`. |
| `cargo` | `cargo` | Keep for a Rust crate. |
| `clippy` | `cargo clippy` | Keep for a Rust crate. |
| `rustfmt` | `cargo fmt` | Keep for a Rust crate. |
| `cargo-deny-advisories` | `cargo deny` | Keep for a Rust crate. |
| `cargo-deny-bans` | `cargo deny` | Keep for a Rust crate. |
| `cargo-deny-licenses` | `cargo deny` | Keep for a Rust crate. |
| `cargo-deny-sources` | `cargo deny` | Keep for a Rust crate. |

Keep `bash` when the repository has `*.sh`, `*.bash`, `*.bats`, or a
file with a `bash` or `sh` shebang. Keep `markdown` when the repository
has Markdown files. The check skips `CHANGELOG.md`. Keep `jactionlint`
when the repository has `.github/workflows/*.yml` or `*.yaml`. The
`mise` check skips when `mise` is absent. Keep the Rust checks when the
repository has `Cargo.toml`. Each Rust check skips and exits 0 when
`cargo` or `Cargo.toml` is absent.

Delete a check script that you do not need. `scripts/ci.sh` runs every
`scripts/check-*.sh` in name order. The script file is the only
registration.

A format script pairs with its check. Copy `format-shfmt.sh` with
`check-shfmt.sh`, `format-dockerfmt.sh` with `check-dockerfmt.sh`, and
`format-dclint.sh` with `check-dclint.sh`.

Some checks print `no ... files` and exit 0 when no file matches. Keep
the check when the repository can gain such a file later.

### Phase 4: Install the hooks

Run this command:

```bash
make hooks
```

The target sets `core.hooksPath` to `.githook`. It also makes the three
hook files executable. Check the value:

```bash
git config --get core.hooksPath
```

The output must be `.githook`. The setting is local to the clone. Each
clone must run `make hooks`.

`make hooks` replaces any existing hook path. Do not run it when the
repository uses Husky, lefthook, or the pre-commit framework. Run
`scripts/ci.sh` and `cog verify` from the existing hooks instead.

Add this command to the existing pre-commit hook and the existing
pre-push hook:

```sh
#!/bin/sh
exec scripts/ci.sh
```

Add this command to the existing commit-msg hook:

```sh
#!/bin/sh
exec cog verify --file "$1"
```

The `pre-commit` and `pre-push` hooks run the full pipeline. A commit
waits for every check. The wait can be long on a large repository.

The committed hooks also change to the repository root first. Keep that
step when your hook tool starts in another directory.

### Phase 5: Add CI

Copy `.github/workflows/ci.yml` and adapt it.

1. Change the `push.branches` list. The committed list is `main` and
    `feature/**`. Use your default branch and your branch patterns.
2. Delete each matrix entry for a check that you dropped.
3. Keep or replace the pinned actions and the `jdxcode/mise` container.
    Each action uses a commit SHA. Each image uses a digest. Keep the
    version comment on each pin.
4. Keep `fetch-depth: 0` on the `commits` checkout step. The commit
    check reads the full history.
5. Keep the `safe.directory` step. The container user differs from the
    checkout owner.
6. Keep `permissions: contents: read`.

A matrix entry has this shape:

```yaml
- name: bash
  install_args: shellcheck
  script: scripts/check-bash.sh
```

The `mise` entry uses `install_args: ''` because it needs no tool.

The committed workflow runs on a push to `main` and `feature/**`, on
each pull request, on demand, and every Monday at 03:00 UTC. Change or
delete the triggers.

The workflow also has four separate jobs:

| Job | When it runs | What it does |
| --- | --- | --- |
| `pr-title` | pull requests | Checks the title. |
| `gitleaks` | every run | Runs the secret scan. |
| `editorconfig` | every run | Runs the editorconfig check. |
| `dependency-review` | pull requests | Reviews dependencies. |

The `gitleaks` job installs `gitleaks` with `mise install gitleaks`.
The `editorconfig` job installs `editorconfig-checker` with
`mise install editorconfig-checker`. A private repository needs GitHub
Advanced Security for `dependency-review`.

`.github/dependabot.yml` is optional. Copy it to keep the pinned action
SHAs current. It updates the `github-actions` ecosystem every week and
groups the updates.

Set the job names that you keep as required status checks in the
repository settings.

### Phase 6: Add the release flow

Copy the release scripts:

```bash
cp -p /tmp/template/scripts/release.sh scripts/release.sh
cp -p /tmp/template/scripts/release-guest.sh scripts/release-guest.sh
```

Copy `cog.toml` and adapt it:

```toml
from_latest_tag = true
ignore_merge_commits = true
tag_prefix = "v"
branch_whitelist = ["main"]

[changelog]
path = "CHANGELOG.md"
template = "default"
```

- `branch_whitelist` lists the branches that permit a bump. Change
  `main` to your release branch.
- `tag_prefix` is `v`. Tags have the form `v1.2.3`.
- `from_latest_tag = true` computes the next version and the changelog
  from the latest tag.
- `ignore_merge_commits = true` leaves merge commits out.
- `[changelog]` sets the path to `CHANGELOG.md` and the default
  template.

Run the release on the branch in `branch_whitelist`:

```bash
make release
```

The target runs `scripts/release.sh`. When smolvm is available, the bump
runs in the sandbox. The guest clones the repository to a writable
directory. The host applies the returned bundle. Without a sandbox, auto
mode prints a warning and runs the bump on the host. Remote mode requires
`SMOLVM_OWNER` and a reachable server. The release does three steps:

1. It updates `CHANGELOG.md`.
2. It creates a version commit.
3. It creates a tag with the `v` prefix.

Do not edit `CHANGELOG.md` by hand. The host validation expects
`CHANGELOG.md` and the `v` tag prefix. `--auto` is the default selector.
A version flag replaces `--auto`.

`.github/workflows/release.yml` runs on a push of a `v*` tag. It creates
a GitHub Release. A local tag does not start the workflow. Push the tag
only when you want the release:

```bash
git push origin v1.2.3
```

The release flow needs Conventional Commits. Use the form
`<type>(<scope>): <description>`. The types are `feat`, `fix`, `docs`,
`test`, `refactor`, `perf`, `build`, `ci`, and `chore`. A `fix` bumps
PATCH. A `feat` bumps MINOR. A `!` or a `BREAKING CHANGE:` footer bumps
MAJOR.

When the existing history does not follow Conventional Commits, do not
enable the commit check and the commit-msg hook yet. Keep
`scripts/check-commits.sh` and `.githook/commit-msg` out of the first
adoption commit. Add them after the history can pass.

When the history follows Conventional Commits and has no tag, create a
local baseline tag at the adoption commit:

```bash
git tag v0.0.0
```

Do not push the tag. The release workflow does not run. The tag gives
`cog` a start point. `scripts/check-commits.sh` then checks only the
commits after the tag.

`--auto` never jumps from `0.y.z` to `1.0.0`. A version flag replaces
`--auto`. Select the first stable version with an explicit command:

```bash
make release ARGS='--version 1.0.0'
```

Run this command to see the next version without a change:

```bash
make release ARGS='--dry-run'
```

### Phase 7: Optional smolvm sandbox

Copy the sandbox files:

```bash
cp -p /tmp/template/scripts/smolvm.sh scripts/smolvm.sh
cp -p /tmp/template/scripts/smolvm-clean.sh scripts/smolvm-clean.sh
cp -p /tmp/template/scripts/smolvm-exec.sh scripts/smolvm-exec.sh
cp -p /tmp/template/scripts/smolvm-resources.sh scripts/smolvm-resources.sh
cp -p /tmp/template/scripts/smolvm-serve.sh scripts/smolvm-serve.sh
mkdir -p smol
cp -p /tmp/template/smol/base.smolfile smol/base.smolfile
cp -p /tmp/template/smol/init.sh smol/init.sh
cp -p /tmp/template/smol/pack-build.sh smol/pack-build.sh
cp -p /tmp/template/smol/preflight.sh smol/preflight.sh
cp -p /tmp/template/smol/verify.sh smol/verify.sh
```

Copy the five `scripts/smolvm*.sh` files together. `smolvm.sh` sources
`smolvm-resources.sh` and `smolvm-serve.sh` from the same directory.

The host needs one of these setups:

- Linux x86_64 or aarch64 with a readable `/dev/kvm`. A cloud instance
  or a virtual machine needs nested virtualization.
- macOS 11 or later on Apple Silicon with `kern.hv_support=1`. This
  repository does not test the macOS path.

The host also needs `smolvm`, `git`, `make`, `sha256sum`, and `bash`.
Install `smolvm` with the vendor script:

```bash
curl -sSL https://smolmachines.com/install.sh | bash
```

Run these commands in order:

```bash
make smolvm-preflight
make smolvm-pack
make smolvm
make smolvm-verify
```

`make smolvm-preflight` reads the host and starts no machine.
`make smolvm-pack` builds the cached pack. `make smolvm` runs the checks
in the guest. `make smolvm-verify` checks the isolation from inside the
guest and from the host.

The guest is offline. It runs a baked pack and asks the registry for
nothing. The script mounts the repository at `/workspace` with the `:ro`
flag. The guest kernel enforces the read-only mount. A check that needs
network access fails in the guest. Fix the check or run it on the host.
The committed check rules do not use the network.

`smol/init.sh` and `mise.toml` must provide every tool. The init script
installs `bash`, `ca-certificates`, `curl`, `git`, and `mise`. It then
runs `mise install` against your `mise.toml`. Add a tool to `mise.toml`
and the next pack build installs it.

The pack cache is `.cache/smolvm/ci-<key>.smolmachine`. The key is the
SHA-256 of the `sha256sum` lines of `mise.toml`, every
`smol/*.smolfile`, `smol/pack-build.sh`, and `smol/init.sh`. The key
also includes `SMOLVM_CPUS` and `SMOLVM_MEMORY_MB` when either value
differs from its default. A change to one of these files or values
builds a new pack.

`SMOLVM_CPUS` sets the CPU count. `SMOLVM_MEMORY_MB` sets the memory in
MiB. The defaults are 1 CPU and 512 MiB. Each value must be a positive
integer.

Set `SMOLVM_ADDR` to use a remote `smolvm serve` server. Set
`SMOLVM_OWNER` to a unique name for your repository. Remote mode
requires `SMOLVM_OWNER`. An empty value stops the command before any
API call. The value uses lowercase letters, digits, hyphen, and
underscore. It starts with a letter or a digit. It has at most 16
characters. The remote path needs `curl` and `tar`. It has no host
fallback. The client copies the working tree and the git metadata to
the server. The server has no authentication by default. Bind it to
loopback or a protected Unix socket. Put a remote server behind an
authenticated TLS proxy. Anyone who can call the API can create machines
and run commands. `SMOLVM_SERVE_TOKEN` sends a bearer token. The scripts
do not print the token.

`make smolvm-pack` refuses to run while `SMOLVM_ADDR` has a value. Unset
the variable before you build a local pack.

`make smolvm-clean` deletes the cached packs and the machines of this
checkout. Every resource name carries an owner. In local mode the owner
is the first 12 hex characters of the SHA-256 of the canonical checkout
path. Set `SMOLVM_OWNER` to use a name instead. Remote mode requires
`SMOLVM_OWNER`. Use one unique value for each repository. The target
prints the plan and asks one time. Use `--dry-run` to print the plan
only:

```bash
make smolvm-clean ARGS='--dry-run'
```

Use `--yes` to skip the prompt:

```bash
make smolvm-clean ARGS='--yes'
```

`SMOLVM_CLEAN_ASSUME_YES=1` also skips the prompt. A non-interactive
run without a skip flag stops with an error. When `SMOLVM_OWNER` is
unset, a moved checkout gets a new token. Its old resources become
foreign and stay.

With a remote address, the target also deletes the cache bases that it
can name. The serve API has no list route for bases. Delete another
base by hand on the server after you check its owner.

`make ci` uses smolvm in auto mode when the binary and the accelerator
work. Otherwise it prints a warning and uses the host. Set
`SMOLVM_ENABLED=1` to require smolvm. The run then exits 3 when smolvm
is unavailable. Set `SMOLVM_ENABLED=0` to force the host path.

## File manifest

| Path | Action | Notes |
| --- | --- | --- |
| `.editorconfig` | merge or copy | Keep your existing rules. |
| `.githook/*` | copy | Copy the three hooks. |
| `.github/workflows/ci.yml` | adapt | Change branches and matrix. |
| `.github/workflows/release.yml` | adapt or skip | Adapt the tag trigger. |
| `.github/dependabot.yml` | optional | Updates the action pins. |
| `Makefile` | merge | Keep your targets. |
| `cog.toml` | adapt | Set your release branch. |
| `mise.toml` | merge | Add the selected tools. |
| `Cargo.toml` | adapt | Set the crate name, the version, and the lints. |
| `Cargo.lock` | copy or adapt | Keep the lock file for a binary crate. |
| `deny.toml` | adapt | Set the advisory, ban, license, and source policy. |
| `src/lib.rs` | adapt | Replace the example library. |
| `src/main.rs` | adapt | Replace the example binary. |
| `scripts/check-cargo.sh` | copy | Runs `cargo test`. |
| `scripts/check-clippy.sh` | copy | Runs `cargo clippy`. |
| `scripts/check-rustfmt.sh` | copy | Runs `cargo fmt --check`. |
| `scripts/check-cargo-deny-advisories.sh` | copy | Runs the advisory check. |
| `scripts/check-cargo-deny-bans.sh` | copy | Runs the ban check. |
| `scripts/check-cargo-deny-licenses.sh` | copy | Runs the license check. |
| `scripts/check-cargo-deny-sources.sh` | copy | Runs the source check. |
| `scripts/format-rustfmt.sh` | copy | Runs `cargo fmt`. |
| `smol/init.sh` | adapt | Add `build-essential` to the apt package list. |
| `scripts/ci.sh` | copy | Runs the check scripts. |
| `scripts/check-*.sh` | copy selected | Copy the Phase 3 checks. |
| `scripts/format*.sh` | copy selected | Copy `format.sh` and pairs. |
| `scripts/release.sh` | copy | Host release orchestrator. |
| `scripts/release-guest.sh` | copy | Guest release worker. |
| `scripts/smolvm*.sh` | optional | Copy the five files. |
| `smol/*` | optional | Copy the five smol files. |
| `.gitignore` | merge | Add `.cache/`, `.smolvm/`, and `target/`. |
| `README.md`, `LICENSE`, `docs/*` | do not copy | Write your own. |
| `.cache/`, `.smolvm/` | do not copy | Add both to `.gitignore`. |

Local release mode needs `scripts/smolvm-resources.sh` from the
`scripts/smolvm*.sh` group. Remote mode needs `scripts/smolvm-serve.sh`.
That file sources the resources file. Remote mode also requires
`SMOLVM_OWNER`. Host mode needs neither.

## Check the setup

1. Run each selected check directly. Add one line for each script that
    you copied:

    ```bash
    scripts/check-bash.sh
    scripts/check-shfmt.sh
    scripts/check-markdown.sh
    ```

    A check exits non-zero on a problem.

2. Run the Rust checks. Run the advisory check with network access:

    ```bash
    make check-cargo
    make check-clippy
    make check-rustfmt
    make check-cargo-deny-bans
    make check-cargo-deny-licenses
    make check-cargo-deny-sources
    make format-rustfmt
    ```

    `make format-rustfmt` changes files. Review the diff before you
    commit. Run `make check-cargo-deny-advisories` with network access.

3. Force the host path first:

    ```bash
    SMOLVM_ENABLED=0 make ci
    ```

    This command skips the microVM detection. It prints no warning. It
    shows whether the tools and the files pass on the host.

4. Apply the formatters, then review the diff:

    ```bash
    make format
    git diff --stat
    git diff
    ```

    The formatters change files. Review every change before you commit.

5. Install the hooks:

    ```bash
    make hooks
    ```

6. Make one test commit. The commit-msg hook runs `cog verify`. The
    pre-commit hook runs `scripts/ci.sh`:

    ```bash
    git commit --allow-empty -m 'chore: test the hooks'
    ```

    Delete the test commit with `git reset --soft HEAD~1` when you do not
    want it.

7. Optional sandbox check:

    ```bash
    make smolvm-preflight
    make smolvm-pack
    make smolvm
    make smolvm-verify
    ```

8. Optional remote check. Use your server address and a unique owner:

    ```bash
    SMOLVM_OWNER=my-repo SMOLVM_ADDR=https://smolvm.example.com make smolvm
    ```

    Every remote command needs `SMOLVM_OWNER`. Use a different value for
    each repository. An empty value stops the command before any API
    call. The remote path needs `curl` and `tar`. It has no host
    fallback.

## Rollback

Delete the `core.hooksPath` setting:

```bash
git config --unset core.hooksPath
```

This command fails when the key is absent. That result is safe to
ignore. When the repository had a hook path before adoption, set it
again:

```bash
git config core.hooksPath <old-path>
```

Delete the copied files. Delete only the files that you added:

```bash
rm -f scripts/ci.sh scripts/format.sh
rm -f scripts/check-bash.sh scripts/check-markdown.sh
rm -f scripts/format-shfmt.sh
rm -rf .githook
rm -rf .cache/smolvm .smolvm
```

Replace the example names with the files that you copied. Do not use a
wildcard when the repository had a file with the same name before
adoption.

Revert the merged files from your backup:

```bash
cp -p Makefile.before-adopt Makefile
cp -p mise.toml.before-adopt mise.toml
cp -p .gitignore.before-adopt .gitignore
cp -p .editorconfig.before-adopt .editorconfig
cp -p cog.toml.before-adopt cog.toml
cp -p .github/workflows/ci.yml.before-adopt .github/workflows/ci.yml
```

Delete the backup files after the rollback. Use `git revert` on the
adoption commit when the adoption is one commit.

## Troubleshooting

### A tool is missing

A check prints a line like this:

```text
error: shellcheck is not on PATH. Install shellcheck first.
```

Install the tool with `mise install`. Or add the tool to the config:

```bash
mise use shellcheck@0
```

Use the major version from `mise.toml`. You can also delete the check
script. Keep each tool version in `mise.toml`.

### A Rust check skips or fails

The Rust scripts skip and exit 0 when `cargo` or `Cargo.toml` is
absent. Install Rust with `mise install` when `mise.toml` lists `rust`.
A Rust build also needs a C toolchain. Install `build-essential` on
Debian or Ubuntu.

The cargo-deny advisories check needs the crates.io registry and the
RustSec advisory database. Run that check on a host with network
access. The local smolvm pack path is offline and cannot download the
database.

### The hook does not run

Run these commands:

```bash
make hooks
git config --get core.hooksPath
```

The output must be `.githook`. Each clone needs `make hooks`. When the
output is another path, an existing hook tool owns the path. Do not
overwrite it. Add `scripts/ci.sh` and `cog verify` to that tool's
hooks.

### check-commits fails on old history

`scripts/check-commits.sh` runs `cog check --from-latest-tag` when a tag
exists. Without a tag it runs `cog check ..HEAD`. The check reads the
full history.

Create a local baseline tag at the adoption commit:

```bash
git tag v0.0.0
```

Do not push the tag. The check then reads only the commits after it.
You can also delete `scripts/check-commits.sh` until the history can
pass. The commit-msg hook still rejects a new message that does not
follow the format. Do not use `--no-verify`.

### check-editorconfig reports many existing files

The check reads every tracked and untracked file that git lists. Fix the
reported files. Or narrow the file list in the copied script:

```bash
mapfile -d '' -t files < <(
  git ls-files -z --cached --others --exclude-standard -- '*.sh' '*.md'
)
```

This example checks only shell and Markdown files. Keep the new files
inside the selection.

### gitleaks finds an existing secret

Rotate the secret first. The working tree holds the secret in plain text.
Then delete the value from the file or replace it with a placeholder. When
the value is a test value, add a narrow allowlist to a gitleaks config.
Pass that config to `gitleaks` in the copied check script.

The check scans a temporary copy of the working tree with
`gitleaks detect --no-git`. It does not scan the git history. A secret
that exists only in old commits stays out of this check.

### smolvm is unavailable

Auto mode prints a warning and uses the host. That is the committed
behavior. To require the sandbox, run:

```bash
SMOLVM_ENABLED=1 make ci
```

The run exits 3 when smolvm or the accelerator is missing. The checks
do not run on the host. Install smolvm and fix the accelerator. You can
also set `SMOLVM_ADDR` for a remote run.

### A remote cache base stays after clean

`make smolvm-clean` deletes only the bases that it can name from the
current checkout and repository state. The serve API has no list route
for bases. A base of another owner or another state stays. Check its
owner, then delete it by hand on the server. The name is
`smolvm-ci-<owner>-<key>.qcow2`. In local mode the owner is the first
12 hex characters of the SHA-256 of the canonical checkout path. Remote
mode requires `SMOLVM_OWNER`. The seed base always stays.

### make uses /bin/sh and fails

Add the header to the Makefile:

```make
SHELL := bash
.DEFAULT_GOAL := help
```

The recipes use bash syntax. Without the header, `make` uses `/bin/sh`.

## Rust add-on

This add-on adds a minimal Rust project and its checks to an existing repository.
The project has one library and one binary. The checks cover the tests,
clippy, rustfmt, and cargo-deny.

### Files

Copy or adapt these files:

| Path | Action | Notes |
| --- | --- | --- |
| `Cargo.toml` | adapt | Set the crate name, the version, and the lints. |
| `Cargo.lock` | copy or adapt | Keep the lock file for a binary crate. |
| `deny.toml` | adapt | Set the advisory, ban, license, and source policy. |
| `src/lib.rs` | adapt | Replace the example library. |
| `src/main.rs` | adapt | Replace the example binary. |
| `scripts/check-cargo.sh` | copy | Runs `cargo test`. |
| `scripts/check-clippy.sh` | copy | Runs `cargo clippy`. |
| `scripts/check-rustfmt.sh` | copy | Runs `cargo fmt --check`. |
| `scripts/check-cargo-deny-advisories.sh` | copy | Runs the advisory check. |
| `scripts/check-cargo-deny-bans.sh` | copy | Runs the ban check. |
| `scripts/check-cargo-deny-licenses.sh` | copy | Runs the license check. |
| `scripts/check-cargo-deny-sources.sh` | copy | Runs the source check. |
| `scripts/format-rustfmt.sh` | copy | Runs `cargo fmt`. |

The starter files are an example. Change the crate name, the module
paths, and the source layout to match the existing crate. Keep the
check scripts as they are, or change the paths inside them.

### Makefile

Add these targets to the `Makefile`:

| Target | Recipe |
| --- | --- |
| `check-cargo` | `scripts/check-cargo.sh` |
| `check-clippy` | `scripts/check-clippy.sh` |
| `check-cargo-deny-advisories` | `scripts/check-cargo-deny-advisories.sh` |
| `check-cargo-deny-bans` | `scripts/check-cargo-deny-bans.sh` |
| `check-cargo-deny-licenses` | `scripts/check-cargo-deny-licenses.sh` |
| `check-cargo-deny-sources` | `scripts/check-cargo-deny-sources.sh` |
| `check-rustfmt` | `scripts/check-rustfmt.sh` |
| `format-rustfmt` | `scripts/format-rustfmt.sh` |

Add the eight names to the `.PHONY` list.

### mise

Add these tools to `mise.toml`:

```toml
rust = "stable"
cargo-deny = "0"
```

### CI

Add these matrix entries to `.github/workflows/ci.yml`:

```yaml
- name: cargo
  install_args: rust
  script: scripts/check-cargo.sh
- name: clippy
  install_args: rust
  script: scripts/check-clippy.sh
- name: rustfmt
  install_args: rust
  script: scripts/check-rustfmt.sh
- name: cargo-deny-advisories
  install_args: "rust cargo-deny"
  script: scripts/check-cargo-deny-advisories.sh
- name: cargo-deny-bans
  install_args: "rust cargo-deny"
  script: scripts/check-cargo-deny-bans.sh
- name: cargo-deny-licenses
  install_args: "rust cargo-deny"
  script: scripts/check-cargo-deny-licenses.sh
- name: cargo-deny-sources
  install_args: "rust cargo-deny"
  script: scripts/check-cargo-deny-sources.sh
```

Add this step before the tool install step:

```yaml
- name: Install a C toolchain for Rust checks
  if: contains(matrix.install_args, 'rust')
  run: |
    apt-get update
    apt-get install -y --no-install-recommends build-essential
```

Add the new job names to the required status checks.

### Other files

Add this line to `.gitignore`:

```text
target/
```

Add `build-essential` to the apt package list in `smol/init.sh`. The
local pack build needs a C toolchain to build Rust crates.

### Check behavior

`check-cargo` runs `cargo test`. `check-clippy` runs
`cargo clippy --all-targets -- -D warnings`. `check-rustfmt` runs
`cargo fmt --all -- --check`. The four cargo-deny scripts run
`cargo deny --all-features check advisories`, `bans`, `licenses`, and
`sources`.

Each script skips and exits 0 when `cargo` or `Cargo.toml` is absent.
A skipped check is not a failure.

### Network

`cargo test`, clippy, and cargo-deny can need the crates.io registry.
The advisories check also needs the RustSec advisory database.

The local pack path is offline. The advisories check cannot download
the RustSec database on that path. The remote `SMOLVM_ADDR` path has
network. Run the advisories check on the host or on the remote path.

### Rust verification

Run these commands:

```bash
make check-cargo
make check-clippy
make check-rustfmt
make check-cargo-deny-bans
make check-cargo-deny-licenses
make check-cargo-deny-sources
make format-rustfmt
```

Run this command with network access:

```bash
make check-cargo-deny-advisories
```

## Limits

- This guide does not rewrite history. Old commits stay as they are.
- It does not replace your existing CI. You adapt the workflow and
  keep your jobs.
- It does not cover uncommitted files. Commit or stash your work first.
- The smolvm guest is offline. The guest mounts the repository read-only.
  A check that needs network access or a write fails in the guest.
- The local smolvm pack path is offline. The Rust cargo-deny advisories
  check cannot download the RustSec database there. Run that check on the
  host or through `SMOLVM_ADDR`, where the network is available.
