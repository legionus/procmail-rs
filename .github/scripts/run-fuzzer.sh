#!/bin/sh
# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

set -eu

total_time=15
max_len=4096

categories="$(sed -n -r -e '/\[\[bin\]\]/,/^$/ { s#name = "([^"]+)"#\1#p }' fuzz/Cargo.toml)"

for c in $categories; do
	cargo +nightly fuzz build "$c"

	case "$c" in
		shell-*)
			dict="fuzz/dictionaries/shell.dict"
			;;
		*)
			dict="fuzz/dictionaries/$c.dict"
			;;
	esac

	[ -f "$dict" ] || dict=

	cargo +nightly fuzz run "$c" -- \
		-max_total_time=$total_time -max_len=$max_len \
		${dict:+-dict="$dict"}
done
