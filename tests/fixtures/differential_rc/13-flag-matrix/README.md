<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Supported recipe flag matrix

This fixture records the behavior of every recipe flag accepted by
procmail-rs. `H`, `B`, and `HB` select condition areas; `D` disables case
folding; `c`, `A`, `a`, `E`, and `e` exercise selection and sibling control.
The command artifacts preserve the exact input selected by `h`, `b`, and
`hb`, including the different ending selected by `r`. Header-only and
body-only `f` filters rewrite the message in sequence. Successful `i`, `w`,
and `W` actions are covered, while failed `w` and `W` actions select their
following `e` handlers; the `W` failure remains quiet.

The files below `expected.artifacts/` are bytes captured from one reviewed run
of Debian-patched procmail 3.23pre. Empty marker files record control-flow
decisions. Ordinary tests execute only `procmail-rs.rc`.
