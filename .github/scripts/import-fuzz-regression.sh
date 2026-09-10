#!/bin/sh
# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

set -eu

usage()
{
	echo "usage: $0 TARGET ARTIFACT" >&2
	exit 2
}

test "$#" -eq 2 || usage
target=$1
artifact=$2

case "$target" in
	destination-path|header-edit|message|ordered-evaluation|rc|regex|shell-condition|shell-expression|shell-pattern)
		;;
	*)
		echo "unsupported fuzz target: $target" >&2
		exit 2
		;;
esac

test -f "$artifact" || {
	echo "fuzz artifact is not a regular file: $artifact" >&2
	exit 2
}

size=$(wc -c <"$artifact")
test "$size" -le 1048576 || {
	echo "fuzz artifact exceeds the 1048576-byte import limit" >&2
	exit 2
}

name=${artifact##*/}
case "$name" in
	""|*[!A-Za-z0-9._-]*)
		echo "fuzz artifact name contains unsupported characters: $name" >&2
	exit 2
		;;
esac

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
directory=$root/tests/fixtures/fuzz-regressions/$target
destination=$directory/$name.hex
mkdir -p -- "$directory"

temporary=$(mktemp "$directory/.import.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM

# Store a reviewable text representation while preserving every input byte.
# The ordinary unit-test suite decodes this file and calls the same project
# entry point used by the selected libFuzzer target.
od -An -v -tx1 -- "$artifact" | tr -d ' \n' >"$temporary"
printf '\n' >>"$temporary"

if test -e "$destination"; then
	if cmp -s -- "$temporary" "$destination"; then
		echo "fuzz regression already exists: $destination"
		exit 0
	fi
	echo "refusing to replace a different regression: $destination" >&2
	exit 1
fi

chmod 0644 "$temporary"
mv -- "$temporary" "$destination"
trap - EXIT HUP INT TERM
echo "imported fuzz regression: $destination"
