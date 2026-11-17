# Contributing

Bug reports, ideas and pull requests are welcome. This page says how the project is worked
on, so that a change can go in without detours.

## Building and testing

icdu builds with stable Rust on Linux and macOS (the minimum version is `rust-version` in
`Cargo.toml`). There is nothing else to install.

```sh
cargo run -- <PATH>            # the browser; needs a real terminal
cargo run -- <PATH> -o -       # scan without the browser, dump to stdout
cargo test                     # all tests
cargo test <part-of-a-name>    # some tests
```

Before a pull request, these four have to pass. CI runs exactly them, on Linux and on macOS:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

## What a change needs

- **One thing per pull request.** A fix and an unrelated clean-up are two pull requests.
- **A test.** A bug fix comes with a test that fails without the fix. A feature comes with
  tests for what it does and for its edge cases. Tests live at the bottom of the source file
  they test.
- **What cannot be unit-tested is said and checked by hand.** That is mostly what the terminal
  shows. Say in the pull request what you looked at. The browser can be driven in a detached
  `tmux` session (`tmux send-keys`, `tmux capture-pane -p`), which is how most of it is checked.
- **Measurements for anything about speed or memory**: what was measured, on what, before and
  after.
- **A line in `CHANGELOG.md`** under "Unreleased", if a user can notice the change.
- **No new dependency** without a good reason, stated in the pull request. There are four,
  on purpose.

Small commits are preferred, each of which builds and passes the tests, with a subject in the
imperative ("Add …", "Fix …") of at most 72 characters and a body that says why.

Branches are named `feature/<topic>` or `fix/<topic>`.

## Things to know before changing something

`CLAUDE.md` describes how the program is built: the scanner threads, the tree that the
interface thread owns, the rules that keep deleting safe, and what must not be done per frame
or per key press. It is written as notes for a coding agent, and it is the most complete
description there is. `AGENTS.md` has the workflow rules in short form.

Three rules cause most surprises:

- **Deleting is guarded on purpose.** Read "Safety gates" in `CLAUDE.md` before touching
  `delete.rs`, a rescan, or anything that acts on a path.
- **Nothing may walk the whole tree for a key press or a frame.** A tree can have many
  millions of entries, and a directory millions of rows.
- **Flags are one table** (`args::FLAGS`). The parser, `--help`, the config files and the
  option list in the README all come from it, and tests compare them. After adding or changing
  a flag, bring the README's list up to date with

  ```sh
  cargo build --release && tools/sync-readme-options.py
  ```

  and describe the flag in the manual page, `doc/icdu.1`. A test fails if the page lacks an
  option or names one that does not exist.

Where ncdu has an equivalent (a key, a flag, a field of the dump), icdu does what ncdu does.
If you change such behaviour, say in the pull request how ncdu behaves and how you found out.

## The recording in the README

`.github/assets/demo.svg` is made by the scripts in `tools/demo` (they need `tmux`, `strace`
and `python3`). If your change alters what the screen looks like, make it again:

```sh
python3 tools/demo/tree.py /tmp/icdu-demo
tools/demo/record.sh target/release/icdu /tmp/icdu-demo/home .github/assets/demo.svg
rm -r /tmp/icdu-demo
```

## How pull requests go in

When CI is green on both platforms, the maintainer moves `main` forward to the commits of the
pull request as they are. There are no merge or squash commits, so the history of `main` is
the history you wrote. Keep it tidy.

## Making a release

For the maintainer. A release is a tag; a workflow does the rest.

1. In `CHANGELOG.md`, turn "Unreleased" into `## [X.Y.Z] - YYYY-MM-DD`, start a new empty
   "Unreleased" above it, and update the links at the bottom.
2. Set `version = "X.Y.Z"` in `Cargo.toml` and run `cargo build`, so that `Cargo.lock` follows.
3. Land that as a pull request like any other.
4. Tag the commit on `main` as `vX.Y.Z` and push the tag.

The release workflow then checks that the tag is the version in `Cargo.toml` and that the
changelog has a section for it, runs the four gates on the tagged commit, builds the binaries
with `tools/dist/build.sh`, and publishes archives and Debian packages with that section of
the changelog as the notes. If a check fails, nothing is published: fix it, and move the tag.

### The crate on crates.io

The first version has to be published by hand, because crates.io can only be told to trust a
workflow for a crate that exists:

1. `cargo login` with a token from crates.io, then, on the tagged commit,
   `cargo publish --dry-run` and `cargo publish`.
2. On crates.io, in the crate's settings under "Trusted Publishing", add this repository
   with the workflow `release.yml`.
3. In this repository, under Settings → Secrets and variables → Actions → Variables, set
   `PUBLISH_CRATE` to `true`.

From then on the release workflow publishes the crate after the GitHub release, with a token
crates.io issues for that one run. The token from step 1 can be revoked.

### The Homebrew tap

`tools/dist/homebrew-formula.sh VERSION sha256sums.txt` prints the formula for a release: it
installs the prebuilt archive for the machine (macOS and Linux, Intel and ARM) with the
manual page. CI tries it on every change, with the archives that run built.

To offer `brew install anhofmann/tap/icdu`:

1. Create the repository `anhofmann/homebrew-tap` (the name has to start with `homebrew-`).
2. Create a fine-grained token that may write the contents of that one repository, and store
   it in this repository as the secret `HOMEBREW_TAP_TOKEN`.
3. Set the repository variable `HOMEBREW_TAP` to `anhofmann/homebrew-tap`.

From then on the release workflow writes `Formula/icdu.rb` into the tap after each release.
Until then, or by hand: download `sha256sums.txt` of the release, run the script, and commit
its output as `Formula/icdu.rb` in the tap.

### The AUR package

`tools/dist/aur-pkgbuild.sh VERSION sha256sums.txt` prints the PKGBUILD of `icdu-bin`, which
installs the prebuilt static binary for x86_64 and aarch64 with the manual page and the
licence. CI builds, checks (namcap) and installs it in an Arch Linux container on every
change, from the archive that run built.

Publishing needs an account on aur.archlinux.org with an SSH key, and is done by hand:

```sh
git clone ssh://aur@aur.archlinux.org/icdu-bin.git && cd icdu-bin
/path/to/icdu/tools/dist/aur-pkgbuild.sh X.Y.Z sha256sums.txt > PKGBUILD
makepkg --printsrcinfo > .SRCINFO      # on Arch Linux
git add PKGBUILD .SRCINFO && git commit -m "icdu X.Y.Z" && git push
```

## Security problems

Not in a public issue, please: see `SECURITY.md`.
