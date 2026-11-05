<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Changelog

All notable user-visible changes will be recorded here. The format follows
Keep a Changelog, and versions follow Semantic Versioning as described in
`Documentation/ReleasePolicy.md`.

## [Unreleased]

## [0.3.0] - 2026-11-05

- Introduce `formail-rs`, a procmail-compatible companion utility for
  standalone header modification, extraction, unfolding, deduplication,
  RFC 5322 Message-ID generation, and mbox escaping. It shares the
  bounded parsing, byte preservation, and fail-closed guarantees of
  `procmail-rs`, enforces `#![forbid(unsafe_code)]`, and streams message
  bodies in constant O(1) memory.
- Extract shared header manipulation, line unfolding, pattern matching, and
  indexed storage into public interfaces in `procmail_rs::header_edit`, and
  reorganize binaries into a uniform `src/bin/` layout.
- Add structured MIME part inspection via the `multipart` condition and
  `multipart { ... }` block syntax, supporting `%mimetype`, `%disposition`,
  and `%filename` selectors (with RFC 2047 and RFC 2231 decoding). Multi-selector
  blocks match against the same MIME subpart to prevent false positives across
  different parts, bounded to 8 nesting levels and 256 total parts.
- Extend the `address` condition to inspect delivery headers (`Delivered-To`,
  `X-Original-To`, `Envelope-To`) and resent headers (`Resent-From`,
  `Resent-Sender`, `Resent-To`, `Resent-Cc`), and add `%recipient`,
  `%delivered`, and `%sender` aliases for subaddress and alias routing.
- Extend the `identifier` condition to inspect thread headers (`In-Reply-To`,
  `References`, `Thread-Index`) and add the `%thread` alias for discussion
  routing.
- Add synchronized message status editing (`status add|remove|set FLAGS`,
  `status clear`) in `headers { ... }` blocks for the `read`, `flagged`, and
  `replied` flags, synchronizing `Status` and `X-Status` headers and routing
  flagged messages directly into Maildir `cur/` with canonical `:2,<flags>` suffixes.
- Add synchronized message priority editing (`priority high|normal|low|clear`)
  in `headers { ... }` blocks, maintaining `Importance`, `Priority`, and
  `X-Priority` in parallel across mail client conventions.
- Add synchronized keyword editing (`keywords add|remove|set|clear`) across
  `Keywords`, `X-Label`, and `X-Keywords`, and add the structured condition
  `keyword all|FIELD[,FIELD...] ?? REGEX` to match individual tags in first-seen
  order without buffering message bodies.

## [0.2.0] - 2026-10-09

- Preserve the rc filename as well as the line number through runtime
  includes, switches, copied branches, deferred deliveries, commands,
  conditions, and native header actions. Detailed text and JSON traces expose
  bounded, escaped source paths while metadata-only logging continues to hide
  them.
- Report the failed delivery operation, typed I/O cause, failure category,
  publication state, and affected destination consistently through streaming,
  fan-out, and ordered delivery. This distinguishes permanent errors such as a
  Maildir path naming a non-directory from transient failures.
- Report the validated byte size returned by every successful external filter,
  making an empty replacement message visible without logging message data.
- Publish Linux Maildir messages by linking the completed unnamed file directly
  into `new`, avoiding an orphaned intermediate `tmp` entry when publication
  fails.
- Preserve cleanup and failure details when a fan-out delivery is interrupted,
  fails after another destination was published, or reaches a durability error
  after publication.
- Retain validated full-filter output without copying it again and construct
  the normalized HB matching view only if a reached condition needs it. On the
  measured 1 MiB full-filter workload, acceptance time fell from 0.866 ms to
  0.0246 ms.
- Share mapped or filter-owned message bodies across native header edits and
  copied branches. Edited headers now use shared indexed storage with lazy
  serialized and normalized views, while delivery, commands, TRAP, and mbox
  quoting consume borrowed message parts.
- Reduce median peak RSS for eight native header actions on the measured 32 MiB
  workload from about 100 MiB to 36 MiB. Add repeatable memory benchmarks and
  document their process-tree and reference-version limitations.
- Expand composed-operation, failure-injection, concurrent delivery, source
  location, message-sharing, and header-edit fuzz regression coverage.

## [0.1.0] - 2026-09-13

- Compile on 32-bit and 64-bit Unix targets by sharing the named-file Maildir
  backend across non-Linux systems.

- Fold the static `explain` command into `check --explain` and support text or
  JSON plan output without reading a message.

- Support bounded `LOG=value` output and statement-ordered `LOGABSTRACT=no`,
  `yes`, and `all` delivery summaries without exposing message headers.

- Add bounded procmail-style rc parsing and byte-oriented message filtering.
- Add explicit Maildir and mboxrd delivery with selectable durability.
- Add trusted external filters, command substitutions, runtime rc files, and
  bounded configuration controls.
- Add native header editing with `set`, `add`, `prepend`, `remove`, `rename`,
  and `extract` operations.
- Add bounded `address` and `identifier` conditions for matching normalized
  mailbox addresses and `List-Id` values without external header parsers.
- Add repeatable `-a` positional arguments with `$N`, `${N}`, and `$#`
  expansion across runtime rc files.
- Add statement-ordered `SHIFT` support for positional arguments, including
  runtime rc files and branch-local processing.
- Add procmail-compatible standalone `"$@"` forwarding to external programs
  while preserving positional argument boundaries and empty values.
- Add bare-name variable removal while preserving the difference between an
  absent value and an explicitly empty assignment.
- Add the procmail `$$`, `$?`, `$_`, and `$-` special parameters for the
  process id, last command status, current rc-file name, and `LASTFOLDER`.
- Add human-readable and JSON trace formats, session-start records, resolved
  delivery paths, and detailed logging controls.
- Add `filter --dry-run` for evaluating recipes without publishing delivery
  destinations, acquiring locks, or running `TRAP`.
- Start external commands in the active `MAILDIR` without changing the parent
  process directory.
- Support 32-bit and 64-bit Linux targets and 64-bit FreeBSD.

[Unreleased]: https://github.com/legionus/procmail-rs/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/legionus/procmail-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/legionus/procmail-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/legionus/procmail-rs/releases/tag/v0.1.0
