# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

PANDOC ?= pandoc
MAN_SOURCES := Documentation/man/procmail-rs.1.md Documentation/man/procmail-rs.rc.5.md

.PHONY: all man check-man

all: man

man: $(MAN_SOURCES) Documentation/man/generate.sh
	PANDOC='$(PANDOC)' Documentation/man/generate.sh man

check-man: man
	@tmpdir=$$(mktemp -d); \
	trap 'rm -rf "$$tmpdir"' EXIT; \
	PANDOC='$(PANDOC)' Documentation/man/generate.sh "$$tmpdir"; \
	cmp "$$tmpdir/procmail-rs.1" man/procmail-rs.1; \
	cmp "$$tmpdir/procmail-rs.rc.5" man/procmail-rs.rc.5
