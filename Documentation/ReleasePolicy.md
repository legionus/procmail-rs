<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Release and versioning policy

The project is experimental and uses semantic version numbers. Before 1.0,
minor releases may change rc behavior or the CLI, but every such change must be
called out in `CHANGELOG.md` and in the compatibility documentation. Patch
releases are reserved for compatible fixes and documentation corrections.
After 1.0, incompatible CLI or documented rc-language changes require a major
release.

A release candidate must pass the locked formatting, build, lint, unit,
integration, unsafe, license, advisory, fuzz smoke, concurrent delivery, and
fault-path checks listed in `AGENTS.md`. Regex changes additionally require the
release benchmark over identical workloads and several rounds. A passing smoke
run is evidence that exercised cases worked; it is not a claim that every
possible input, filesystem, or process behavior has been explored.

The supported release platform is 32-bit and 64-bit Linux. Non-Linux Unix and
Windows releases remain deferred until their delivery behavior has dedicated
implementation and runtime tests.
