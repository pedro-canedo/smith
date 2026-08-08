#!/usr/bin/env bash
#
# Cuts a release: bump, changelog, gates, commit, tag.
#
# The release workflow refuses a tag whose `[workspace.package] version`
# disagrees with it, and that guard is right — v0.2.1 through v0.2.3 shipped
# binaries reporting 0.2.0, which made `smith update` offer the same update
# forever. But the guard only *catches* the mistake, and a five-step manual
# sequence whose first step is invisible until CI fails will keep making it:
# v0.3.0, v0.3.1 and v0.3.2 were each tagged against a stale manifest.
#
# So this is the sequence, in one command, refusing before it touches
# anything rather than half-way through.
#
# The tag is NOT pushed. Pushing it is what publishes artifacts to the world,
# and that deserves its own deliberate keystroke — the command is printed.
#
#   scripts/release.sh 0.3.2

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

version="${1:-}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "usage: scripts/release.sh <major.minor.patch>" >&2
    echo "  e.g. scripts/release.sh 0.3.2" >&2
    exit 2
fi
tag="v$version"

# --- refuse early, before anything is edited --------------------------------

# A dirty tree is the one thing that turns a release commit into a mystery:
# `git commit -a` would sweep in whatever else was open, and on this repo that
# routinely includes another agent's work in progress.
if [ -n "$(git status --porcelain)" ]; then
    echo "error: the working tree is dirty. Commit or stash first:" >&2
    git status --short >&2
    exit 1
fi

if git rev-parse "$tag" >/dev/null 2>&1; then
    echo "error: tag $tag already exists." >&2
    # A pushed tag is immutable: the repository's "release tags are immutable"
    # ruleset refuses deletion, update and non-fast-forward on refs/tags/v*, so
    # there is no recovering a published version number — the fix is always the
    # next patch. Only a tag that never left this machine can be dropped.
    if git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1; then
        echo "It is already published, and published tags cannot be moved or" >&2
        echo "deleted. Cut the next patch version instead." >&2
    else
        echo "It is local-only, so if it was tagged by mistake you can drop it:" >&2
        echo "  git tag -d $tag" >&2
    fi
    exit 1
fi

current="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
if [ "$current" = "$version" ]; then
    echo "error: Cargo.toml already says $version — nothing to bump." >&2
    echo "Either you meant a different version, or the bump is already committed" >&2
    echo "and you only need: git tag $tag" >&2
    exit 1
fi
echo "Releasing $current -> $version"

# --- edit -------------------------------------------------------------------

# Only the first `version = ` line, which is `[workspace.package]`'s. The
# dependency versions below it must not move.
perl -0pi -e "s/^version = \"\Q$current\E\"\$/version = \"$version\"/m" Cargo.toml

# `## Unreleased` becomes this release, dated. Absent is not an error: a
# release whose notes were already written under a heading is fine, and a
# release with no user-facing change is a legitimate thing to cut.
if grep -q '^## Unreleased' CHANGELOG.md; then
    perl -0pi -e "s/^## Unreleased\$/## $version — $(date -u +%Y-%m-%d)/m" CHANGELOG.md
    echo "CHANGELOG: Unreleased -> $version"
else
    echo "CHANGELOG: no Unreleased section; leaving it alone."
fi

# The lockfile carries every workspace member's version.
cargo update --workspace --quiet

# --- gates ------------------------------------------------------------------
#
# The same four CI runs, in the same order, because finding out from a failed
# release run costs a tag and a force-push.

echo "Running the gates..."
bash scripts/check-file-size.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# --- commit and tag ---------------------------------------------------------

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "Release $version"
git tag "$tag"

echo
echo "Tagged $tag. Nothing has been pushed yet — publish with:"
echo
echo "  git push origin main && git push origin $tag"
echo
