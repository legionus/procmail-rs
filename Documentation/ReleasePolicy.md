<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Release and versioning policy

The project is experimental and uses semantic version numbers. Before 1.0,
minor releases may change rc behavior or the CLI, but every such change must be
called out in `CHANGELOG.md` and in the compatibility documentation. Patch
releases are reserved for compatible fixes and documentation corrections.
After 1.0, incompatible CLI or documented rc-language changes require a major
release.

Rust 1.85.0 is the minimum supported compiler. `Cargo.toml` rejects older
compilers, and CI runs the normal check and test suite with that exact release.
Raising the minimum requires a documented reason, a changelog entry, and an
explicit CI change in the same revision.

A release candidate must pass the locked formatting, build, lint, unit,
integration, unsafe, license, advisory, fuzz smoke, concurrent delivery, and
fault-path checks listed in `AGENTS.md`. Regex changes additionally require the
release benchmark over identical workloads and several rounds. A passing smoke
run is evidence that exercised cases worked; it is not a claim that every
possible input, filesystem, or process behavior has been explored.

Run `make check-man` with Pandoc before packaging a release. The generated
`man/procmail-rs.1` and `man/procmail-rs.rc.5` files must match their Markdown
sources under `Documentation/man/`; release packages install the generated
files and do not need Pandoc at build or installation time.

Run `make check-package` before publishing. It creates a Cargo source archive,
extracts it into an empty directory, runs its tests, and installs the binary
and generated pages into a temporary `DESTDIR`. This check detects release
files that were available only in the working tree or omitted from the source
archive. Perform the final run from a clean checkout of the intended release
revision; `--allow-dirty` exists only so contributors can validate a proposed
change before committing it.

All third-party CI actions are selected by a full commit identifier with a
nearby release-tag comment. Review and update both together rather than using
a moving tag.

The supported release platforms are 32-bit and 64-bit Linux and 64-bit
FreeBSD. Linux delivery is exercised directly by the main CI jobs, while a
native FreeBSD VM job exercises its platform-specific Maildir publication.
Other Unix systems and Windows remain deferred until their delivery behavior
has dedicated implementation and runtime tests.
