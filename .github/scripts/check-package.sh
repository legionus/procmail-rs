#!/bin/sh
# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

set -eu

temporary_directory=$(mktemp -d)
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

# Use a private target directory so a stale archive cannot be mistaken for the
# artifact produced from this tree. Extracting and testing that artifact also
# catches files which exist in a checkout but were omitted from the package.
CARGO_TARGET_DIR="$temporary_directory/target" \
	cargo package --locked --allow-dirty --no-verify
set -- "$temporary_directory"/target/package/procmail-rs-*.crate
if test "$#" -ne 1 || test ! -f "$1"; then
	echo "expected exactly one procmail-rs package archive" >&2
	exit 1
fi
tar -xzf "$1" -C "$temporary_directory"
set -- "$temporary_directory"/procmail-rs-*
if test "$#" -ne 1 || test ! -d "$1"; then
	echo "expected exactly one extracted procmail-rs package" >&2
	exit 1
fi
package_directory=$1

(cd "$package_directory" && cargo test --locked)

# Install into an empty image and check every public artifact and its mode.
# This exercises the same generated manual pages that distributors receive,
# without making Pandoc part of the package build or installation requirements.
installation_root="$temporary_directory/image"
make -C "$package_directory" install DESTDIR="$installation_root" PREFIX=/usr
test -x "$installation_root/usr/bin/procmail-rs"
test "$(stat -c %a "$installation_root/usr/bin/procmail-rs")" = 755
test "$(stat -c %a "$installation_root/usr/share/man/man1/procmail-rs.1")" = 644
test "$(stat -c %a "$installation_root/usr/share/man/man5/procmail-rs.rc.5")" = 644
"$installation_root/usr/bin/procmail-rs" --version
