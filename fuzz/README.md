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
```

Longer release-candidate jobs should retain and review any generated corpus or
crash artifact before adding it to the repository.
