<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Changelog

All notable user-visible changes will be recorded here. The format follows
Keep a Changelog, and versions follow Semantic Versioning as described in
`Documentation/ReleasePolicy.md`.

## Unreleased

- Add bounded procmail-style rc parsing and byte-oriented message filtering.
- Add explicit Maildir and mboxrd delivery with selectable durability.
- Add trusted external filters, command substitutions, runtime rc files, and
  bounded configuration controls.
- Add native header editing with `set`, `add`, `prepend`, `remove`, `rename`,
  and `extract` operations.
- Add human-readable and JSON trace formats, session-start records, resolved
  delivery paths, and detailed logging controls.
- Add `filter --dry-run` for evaluating recipes without publishing delivery
  destinations, acquiring locks, or running `TRAP`.
- Start external commands in the active `MAILDIR` without changing the parent
  process directory.
- Support 32-bit and 64-bit Linux targets and 64-bit FreeBSD.
