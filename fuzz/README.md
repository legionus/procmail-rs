<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Fuzz targets

The fuzz package is deliberately isolated from the release package and has its
own lockfile. `libfuzzer-sys` contains the native libFuzzer runtime and unsafe
bindings, so it is accepted only as a development tool and is never linked into
`procmail-rs`.

Run bounded smoke sessions with nightly Rust:

```text
rustup run nightly cargo fuzz run rc -- -max_total_time=30
rustup run nightly cargo fuzz run message -- -max_total_time=30
rustup run nightly cargo fuzz run shell-expression -- -max_total_time=30 -dict=fuzz/dictionaries/shell.dict
rustup run nightly cargo fuzz run shell-condition -- -max_total_time=30 -dict=fuzz/dictionaries/shell.dict
rustup run nightly cargo fuzz run shell-pattern -- -max_total_time=30 -dict=fuzz/dictionaries/shell.dict
rustup run nightly cargo fuzz run regex -- -max_total_time=30 -dict=fuzz/dictionaries/regex.dict
rustup run nightly cargo fuzz run header-edit -- -max_total_time=30 -dict=fuzz/dictionaries/header-edit.dict
```

The shell targets use deterministic variable and command results. They never
execute command text produced by fuzz input.

The message target treats its first ten bytes as five little-endian limit
selectors and the remainder as the message. It tests each selected limit at
the neighboring values below, at, and above the selector while leaving the
other four limits at their defaults.

The header-edit target parses its operation text as a real `headers` action,
applies it to a separately bounded byte message, and reparses successful output.

Longer release-candidate jobs should retain and review any generated corpus or
crash artifact before adding it to the repository.
