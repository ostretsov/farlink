---
name: release
description: Release a new Farlink version - bump `version` in Cargo.toml (and Cargo.lock), run the tests, commit, tag vX.Y.Z and push to GitHub so CI builds the GitHub Release. Use when asked to bump the version, release, cut or publish a new Farlink version. Argument - patch (default), minor, major, or an exact X.Y.Z.
---

# Bump Farlink's version, commit, tag and push

Pushing a tag `vX.Y.Z` makes `.github/workflows/build.yml` build Linux x86_64/ARM64 + Windows and
publish a GitHub Release. The workflow **fails if the tag differs from `version` in Cargo.toml**, so
the commit and the tag must always go out together.

Argument: `patch` (default), `minor`, `major`, or an exact version like `0.3.0`.

## 1. Check the repository (stop and tell the user if any check fails)

Run everything from `/home/artem/farlink`.

```sh
cd /home/artem/farlink
git rev-parse --show-toplevel   # must print /home/artem/farlink
```

* If it prints `/home/artem` (the home-folder dotfiles repo, which ignores everything) or fails, the
  project has **no repository of its own yet**. Never commit Farlink into the home repo. Stop and offer
  to set it up: `git init -b main`, first commit, `git remote add origin git@github.com:<user>/farlink.git`.
  The user creates the GitHub repo (the `gh` CLI is not installed here).
* `git remote get-url origin` must point to GitHub.
* `git branch --show-current` must be `main`.
* `git fetch origin` and then `git status -sb` must not say `behind`. If it does, stop: the user decides
  whether to pull.
* Uncommitted changes (`git status --short`): show them and ask whether they belong in this release
  commit. Never stage them silently. Files listed in `.gitignore` (`target/`, `dist/`, `shots/`) never go in.

## 2. Work out the new version

```sh
cur=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
```

* patch: `0.1.4 → 0.1.5`. minor: `0.1.4 → 0.2.0`. major: `0.1.4 → 1.0.0`.
* An exact version must be `X.Y.Z` and greater than `$cur`.
* The tag must not exist yet: `git tag -l v$new` and `git ls-remote --tags origin v$new` both print nothing.

Tell the user `$cur → $new` in one line, then continue (they asked for the bump, so there's no need to
ask again).

## 3. Tests must be green

```sh
make test
make check     # Linux + Windows compile check, same as CI
```

If anything fails, stop and report it. Never release a red build.

## 4. Bump

Change only the `version = ` line in the `[package]` section of `Cargo.toml`. It is the first
`^version = ` line in the file; dependency versions are inline tables and never start a line.

```sh
sed -i "0,/^version = \".*\"/s//version = \"$new\"/" Cargo.toml
cargo update --workspace --offline   # updates only farlink's own entry in Cargo.lock
git diff --stat                      # expect Cargo.toml and Cargo.lock only (plus files the user approved)
git diff Cargo.lock                  # exactly one line: farlink's version
```

If `--offline` fails, run `cargo update --workspace` without it. If Cargo.lock changes anything besides
the `farlink` package version, undo with `git checkout Cargo.lock` and use `cargo check --offline` instead.

Release notes: if `README.md` or other docs show a specific version number, update it too
(`grep -rn "$cur" --include='*.md' .`).

## 5. Commit, tag, push

```sh
git add Cargo.toml Cargo.lock   # + any files the user approved in step 1
git commit -m "Release v$new" -m "<attribution lines for this session>"
git tag -a "v$new" -m "Farlink $new"
git push --atomic origin main "v$new"
```

* End the commit message with the attribution lines the current session tells you to use for commits.
* `--atomic` pushes the branch and the tag together, or neither.
* If the push is rejected, do **not** force-push. Leave the local commit and tag in place, report the
  error, and let the user decide (after a pull, recreate the tag on the new commit with `git tag -f`).

## 6. Report

In a few lines:

* `$cur → $new`, the commit hash, the tag.
* CI: `https://github.com/<owner>/<repo>/actions`. The release appears at
  `https://github.com/<owner>/<repo>/releases/tag/v$new` once the build finishes (about 15-20 minutes).
  Get `<owner>/<repo>` from `git remote get-url origin`.
* Servers installed with `--github OWNER/REPO` (`update_from_github` in their config) pick the release
  up within an hour of it appearing: they update themselves, then the computers. Other servers need
  `farlink-server publish $new <dir>` by hand.
* If a release asset is renamed in `build.yml`, `src/server_update.rs` (`ASSETS`) must change with it,
  or servers stop finding it.
