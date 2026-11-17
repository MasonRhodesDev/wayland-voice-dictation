#!/bin/bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
NAME=wayland-voice-dictation
SPEC="$REPO/packaging/$NAME.spec"
SOURCES="${HOME}/rpmbuild/SOURCES"
COPR_PROJECT="${COPR_PROJECT:-$NAME}"

VER=$(sed -n 's/^Version:[[:space:]]*//p' "$SPEC")
CARGO_VER=$(awk -F'"' '/^\[workspace\.package\]/{ws=1; next} /^\[/{ws=0} ws && /^version[[:space:]]*=/{print $2; exit}' "$REPO/Cargo.toml")
PKGBUILD_VER=$(sed -n 's/^pkgver=//p' "$REPO/packaging/PKGBUILD")
LOCK_VER=$(awk '/^name = "voice-dictation"$/{getline; gsub(/version = "|"/,""); print; exit}' "$REPO/Cargo.lock")
mismatch=""
[ "$CARGO_VER" = "$VER" ] || mismatch="$mismatch\n  Cargo.toml [workspace.package]=$CARGO_VER"
[ "$PKGBUILD_VER" = "$VER" ] || mismatch="$mismatch\n  PKGBUILD pkgver=$PKGBUILD_VER"
[ "$LOCK_VER" = "$VER" ] || mismatch="$mismatch\n  Cargo.lock=$LOCK_VER"
if [ -n "$mismatch" ]; then
    echo "ERROR: version mismatch (spec Version=$VER):$(printf "$mismatch")" >&2
    echo "Bump spec, Cargo.toml [workspace.package], PKGBUILD pkgver, and Cargo.lock together." >&2
    exit 1
fi

REF="v$VER"
if [ "${1:-}" = "--head" ]; then
    REF="HEAD"
    echo "WARNING: building from HEAD (testing only)"
    shift
elif ! git -C "$REPO" rev-parse -q --verify "refs/tags/$REF" >/dev/null; then
    echo "ERROR: tag $REF not found — tag the release first (or use --head to test)" >&2
    exit 1
fi

mkdir -p "$SOURCES"
echo "==> source tarball from $REF"
git -C "$REPO" archive --format=tar.gz --prefix="$NAME-$VER/" \
    -o "$SOURCES/$NAME-$VER.tar.gz" "$REF"

echo "==> vendoring cargo dependencies (crates.io + git: schema-tui, ksni)"
VENDOR_DIR=$(mktemp -d)
trap 'rm -rf "$VENDOR_DIR"' EXIT
git -C "$REPO" archive --prefix=src/ "$REF" | tar -x -C "$VENDOR_DIR"
(cd "$VENDOR_DIR/src" && cargo vendor --locked ../vendor > ../vendor-config.toml)
awk '/^\[source\."git\+/{p=1} /^\[source\.crates-io\]/{p=0} /^\[source\.vendored-sources\]/{p=0} p' \
    "$VENDOR_DIR/vendor-config.toml" > "$VENDOR_DIR/vendor-git-sources.toml"
tar -cJf "$SOURCES/$NAME-$VER-vendor.tar.xz" -C "$VENDOR_DIR" vendor vendor-git-sources.toml

echo "==> building SRPM"
SRPM=$(rpmbuild -bs "$SPEC" | sed -n 's/^Wrote: //p')
echo "    $SRPM"
rpmlint --rpmlintrc "$REPO/packaging/$NAME.rpmlintrc" "$SRPM"

if [ "${1:-}" = "--copr" ]; then
    echo "==> submitting to COPR project $COPR_PROJECT"
    if ! copr-cli build "$COPR_PROJECT" "$SRPM"; then
        echo "ERROR: copr build failed. If this was a 401, the API token has" >&2
        echo "expired (~180 days) — renew at https://copr.fedorainfracloud.org/api/" >&2
        exit 1
    fi
fi
