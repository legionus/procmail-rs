# SPDX-License-Identifier: MIT
# Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

PANDOC ?= pandoc
CARGO ?= cargo
INSTALL ?= install
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
MANDIR ?= $(PREFIX)/share/man
DESTDIR ?=
MAN_SOURCES := Documentation/man/procmail-rs.1.md Documentation/man/procmail-rs.rc.5.md

.PHONY: all build-release man check-man install check-package

all: man

build-release:
	$(CARGO) build --locked --release

man: $(MAN_SOURCES) Documentation/man/generate.sh
	PANDOC='$(PANDOC)' Documentation/man/generate.sh man

check-man: man
	@tmpdir=$$(mktemp -d); \
	trap 'rm -rf "$$tmpdir"' EXIT; \
	PANDOC='$(PANDOC)' Documentation/man/generate.sh "$$tmpdir"; \
	cmp "$$tmpdir/procmail-rs.1" man/procmail-rs.1; \
	cmp "$$tmpdir/procmail-rs.rc.5" man/procmail-rs.rc.5

install: build-release
	@test -f man/procmail-rs.1 -a -f man/procmail-rs.rc.5
	$(INSTALL) -d '$(DESTDIR)$(BINDIR)' '$(DESTDIR)$(MANDIR)/man1' \
		'$(DESTDIR)$(MANDIR)/man5'
	$(INSTALL) -m 0755 target/release/procmail-rs \
		'$(DESTDIR)$(BINDIR)/procmail-rs'
	$(INSTALL) -m 0644 man/procmail-rs.1 \
		'$(DESTDIR)$(MANDIR)/man1/procmail-rs.1'
	$(INSTALL) -m 0644 man/procmail-rs.rc.5 \
		'$(DESTDIR)$(MANDIR)/man5/procmail-rs.rc.5'

check-package:
	.github/scripts/check-package.sh
