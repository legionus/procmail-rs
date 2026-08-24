<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# License review

The project source, documentation, examples, tests, and stored fixtures are
released under MIT as stated in `LICENSE` and their SPDX notices.

The differential fixtures were written for this project. Their `procmail.rc`
files record configurations used to observe Debian-patched procmail behavior;
the paired `procmail-rs.rc` files express the same scenarios. Expected status,
destination, trace, and message files record reviewed outcomes and test data.
They do not contain copied procmail implementation source, and ordinary tests
do not build or execute the reference project.

The locked production dependency tree was reviewed with `cargo deny`. Its
declared choices are compatible with MIT distribution:

| Packages | Declared choice |
| --- | --- |
| `procmail-rs` | MIT |
| `libc`, `regex`, `regex-automata`, `regex-syntax`, `bitflags`, `errno`, `windows-link`, `windows-sys` | MIT or Apache-2.0 |
| `aho-corasick`, `memchr` | Unlicense or MIT |
| `rustix`, `linux-raw-sys` | Apache-2.0 with LLVM exception, Apache-2.0, or MIT |

`deny.toml` admits only MIT, Apache-2.0, Apache-2.0 with LLVM exception, and
Unlicense expressions. `cargo deny check licenses` must be rerun whenever the
lockfile or target dependency graph changes. Fuzzing has a separate manifest
and lockfile; its dependencies are development tools and are not linked into
the release executable.
