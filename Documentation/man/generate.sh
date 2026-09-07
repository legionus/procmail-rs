#!/bin/sh
# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

set -eu

pandoc_command=${PANDOC:-pandoc}
output_directory=${1:-man}

mkdir -p "$output_directory"

generate_page()
{
	source=$1
	output=$2
	title=$3
	section=$4
	footer=$5
	temporary=$(mktemp)

	# Pandoc records its own version in the first two comments. Replace those
	# volatile lines with project licensing so output stays identical across
	# supported Pandoc releases and every installed page carries its provenance.
	trap 'rm -f "$temporary"' EXIT HUP INT TERM
	"$pandoc_command" --standalone --from=markdown --to=man \
		--metadata="title:$title" --metadata="section:$section" \
		--metadata=header:procmail-rs --metadata="footer:$footer" \
		--output="$temporary" "$source"
	{
		printf '.\\" SPDX-License-Identifier: MIT\n'
		printf '.\\" Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>\n'
		sed '1,2d' "$temporary"
	} >"$output"
	rm -f "$temporary"
	trap - EXIT HUP INT TERM
}

generate_page Documentation/man/procmail-rs.1.md \
	"$output_directory/procmail-rs.1" PROCMAIL-RS 1 'User Commands'
generate_page Documentation/man/procmail-rs.rc.5.md \
	"$output_directory/procmail-rs.rc.5" PROCMAIL-RS.RC 5 'File Formats Manual'
