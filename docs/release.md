# Release workflow

This document describes the release workflow. The repository uses
[`Cocogitto`](https://docs.cocogitto.io/) to check commits, write the
changelog, and bump the version. The command name is `cog`.

## Quick start

Run these commands on `main`:

```bash
cog bump --dry-run --auto
make release
```

The first command shows the next version and changes nothing. The
second updates `CHANGELOG.md`, creates a version commit, and creates a
tag with the `v` prefix. See [Make a release](#make-a-release) for the
full steps.

## Commit format

Write every commit subject in this form:

```text
<type>(<scope>): <description>
```

`<type>` is one of these values: `feat`, `fix`, `docs`, `test`,
`refactor`, `perf`, `build`, `ci`, `chore`.

`<scope>` is optional. Use a short name for the changed area, for
example `ci` or `hooks`. `<description>` is a short sentence in the
imperative mood. Do not end the subject with a period.

A commit that breaks compatibility adds `!` before the colon:

```text
feat(api)!: remove the old flag
```

A `BREAKING CHANGE:` footer also marks a breaking change.

## Enforcement

The repository enforces the commit format in three places:

- `.githook/commit-msg` runs `cog verify --file` on the message before
  Git creates the commit. A bad message stops the commit.
- `scripts/check-commits.sh` runs `cog check` from `scripts/ci.sh`.
  `make ci` and both Git hooks run this script.
- `.githook/pre-push` runs `scripts/ci.sh` before each push. A bad
  commit in the history stops the push.

Do not use `--no-verify` to skip these checks. The
[check pipeline](ci.md) describes the scripts.

## Changelog

`cog bump` generates `CHANGELOG.md`. Do not edit the file by hand. The
Markdown check skips this file. The file has no entry until the first
release.

## Versioning

`cog` selects the next version from the commit types:

- `fix` bumps PATCH.
- `feat` bumps MINOR.
- `!` or `BREAKING CHANGE:` bumps MAJOR.

Tags have the `v` prefix, for example `v1.2.3`.

`cog bump --auto` never jumps from `0.y.z` to `1.0.0`. The `0.y.z`
series marks a version before the first stable release.

## Make a release

Run the release on the `main` branch. `cog.toml` permits a bump on that
branch only. Run this command:

```bash
make release
```

The target runs `scripts/release.sh`. That script runs `cog bump` on the
host or in a sandbox. `--auto` is the default selector. A version flag
replaces `--auto`. The command has three steps:

1. It updates `CHANGELOG.md`.
2. It creates a version commit.
3. It creates a tag with the `v` prefix.

No CI release job exists. The release runs on the machine that runs
`make release`. When smolvm is available, the bump runs in a sandbox
microVM. See [Sandbox release flow](#sandbox-release-flow).

## Sandbox release flow

`scripts/release.sh` uses the same execution modes as the checks:

- `SMOLVM_ENABLED=0` runs `cog bump --auto` on the host. The script
  uses no sandbox.
- `SMOLVM_ENABLED=1` requires smolvm. The script uses a local machine
  or a remote server. When neither is available, it prints an error and
  exits 3.
- An unset value or any other value selects auto mode. The script uses
  the remote server when `SMOLVM_ADDR` is set. It uses a local machine
  when `smolvm` is on `PATH` and the accelerator works. Otherwise it
  prints a warning and runs `cog bump --auto` on the host.

Local mode needs a normal checkout, not a linked worktree. The
repository mount must contain the git directory. Remote mode stages the
git data and can use a linked worktree.

In local mode, the script mounts the repository at `/workspace` with
the `:ro` flag. The mount stays read-only. The guest runs
`scripts/release-guest.sh`. That script does these steps:

1. It clones `/workspace` with `git clone --no-hardlinks` to a
    writable temp directory.
2. It removes the `origin` remote.
3. It checks out the host branch at the captured HEAD.
4. It sets the hooks path, the user name, and the user email.
5. It runs `cog bump` with the arguments from the host.

The guest runs no second sandbox. The pre-commit hook runs
`scripts/ci.sh` with `SMOLVM_ENABLED=0`, so the checks do not recurse.

The guest builds a git bundle. The bundle holds the new version commit
and the tag. The guest prints the bundle as base64 between two markers.
The host reads the markers and removes them from the printed output.

Before the host changes a ref, it checks these items:

- The bundle verifies.
- The bundle holds exactly one `v*` tag.
- The tag does not exist on the host.
- The commit has one parent, and the parent is the captured HEAD.
- The commit adds one commit.
- The commit changes only `CHANGELOG.md`.
- The author and committer email match the host email.
- The host HEAD, branch, and worktree still match the preflight state.

The host fetches the tag from the bundle. It applies the commit with
`git merge --ff-only`. It creates no merge commit. A failure after the
fetch deletes the fetched tag. It resets the host to the captured HEAD
only when the host is on the captured branch at the guest commit and
the worktree is clean. Otherwise it leaves HEAD and the worktree alone.
A user commit or a dirty file from the guest run stays.

The tag stays local. Push it with `git push origin v1.2.3` when you
want the GitHub Release workflow to run.

A guest run that makes no commit prints `SMOLVM_RELEASE_NONE=1`. The
host prints a short message and exits 0. This covers `--dry-run` and a
history with no releasable commits.

Remote mode uses `scripts/smolvm-serve.sh` and the `serve_run_remote`
helper. It uses no new transport. The guest command has the same shape.
Remote mode requires `SMOLVM_OWNER` and a reachable address. The owner
rules and the resource defaults come from the smolvm helpers. See the
[smolvm documentation](smolvm.md#remote-mode).

## Optional GitHub Release

A GitHub Release is optional. The local `make release` flow stays the
default. It updates the changelog, creates the version commit, and creates
the tag. It does not create a GitHub Release.

The workflow `.github/workflows/release.yml` creates the release. It runs
only when you opt in. To opt in, push the version tag to GitHub. The tag
must match `v*`, for example `v1.2.3`. Push the tag that `make release`
created:

```bash
git push origin v1.2.3
```

The workflow then creates a GitHub Release for that tag. It uses the
changelog section for that version as the release notes. If the changelog
has no section for the version, the workflow uses the tag name as the
release title.

To stay local, do not push the tag. The workflow does not run. No GitHub
Release is created.

## Dry run

Run this command on the `main` branch to see the next version:

```bash
cog bump --dry-run --auto
```

The command prints the next version and changes nothing. It does not
write the changelog, create a commit, or create a tag.

Run the dry run through the release path to test the sandbox:

```bash
make release ARGS='--dry-run'
```

The guest makes no commit. It prints `SMOLVM_RELEASE_NONE=1`. The host
prints a short message and exits 0. The host repository does not change.

## First release

The repository has no tag. Select the first version before the first
release. `--auto` is the default selector. A version flag replaces
`--auto`. Run one of these commands:

```bash
make release ARGS='--version 1.0.0'
make release ARGS='--version 0.1.0'
```

The maintainer decides the first version. Use `1.0.0` when the public
interface is stable. Use `0.1.0` when the interface can change. The
flags `--major`, `--minor`, and `--patch` also replace `--auto`.

## Limits

The release path passed a simulation with a fake `cog`. The remote
release path ran on `smolvm serve` v1.35.5 with `SMOLVM_SERVE_CACHE=0`.
The guest ran a cold init with real cocogitto 7.0.0. A live local
`smolvm machine run` release has not run. The pack build and cache reuse
path for a release is not tested.

The check suite checks the shell scripts. It does not run a bump. Test
the path with a dry run on the release branch before the first release.
The dry run makes no commit.

## Troubleshooting

### `cog` is not on `PATH`

The `cog` command must be on `PATH`. The check prints this message when
the tool is absent:

```text
error: cog is not on PATH. Install cocogitto first.
```

### The commit hook rejects a message

The commit hook runs `cog verify`. Read the error. Write the message in
the commit format. Run this command to correct the last commit:

```bash
git commit --amend
```

Do not use `--no-verify`.

### The Markdown check skips `CHANGELOG.md`

`cog bump` generates the file. The file format can disagree with the
Markdown rules. `scripts/check-markdown.sh` skips the file on purpose.
Do not add the file to the check.
