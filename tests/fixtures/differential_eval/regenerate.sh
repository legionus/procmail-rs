#!/bin/sh
# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
fixtures=$root/tests/fixtures/differential_eval
reference=$root/external/procmail-3.22/new/procmail

for tool in make cc timeout mktemp find sort; do
	command -v "$tool" >/dev/null 2>&1 || {
		echo "missing required tool: $tool" >&2
		exit 1
	}
done

make -s -C "$root/external/procmail-3.22" CFLAGS0='-O -std=gnu89'
test -x "$reference"

work=$(mktemp -d "${TMPDIR:-/tmp}/procmail-rs-reference.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT HUP INT TERM
chmod 700 "$work"

case_list=$work/cases
find "$fixtures" -mindepth 1 -maxdepth 1 -type d -print | LC_ALL=C sort > "$case_list"
while IFS= read -r case_dir; do
	case_name=${case_dir##*/}
	case_work=$work/$case_name
	mkdir -m 700 "$case_work"

	# Every matching copy recipe creates one distinct mailbox. The final sink
	# consumes the original so no implicit system mailbox can be reached.
	if ! env -i HOME="$case_work" LOGNAME=reference USER=reference \
		PATH=/usr/bin:/bin LC_ALL=C TZ=UTC \
		timeout 30 "$reference" -m "$case_dir/procmail.rc" \
		"$case_work" < "$case_dir/message.eml"; then
		echo "reference procmail failed for $case_name" >&2
		exit 1
	fi

	find "$case_work" -maxdepth 1 -type f -printf '%f\n' |
		LC_ALL=C sort > "$case_dir/expected.destinations.new"
	mv -- "$case_dir/expected.destinations.new" \
		"$case_dir/expected.destinations"
done < "$case_list"
