# Differential runtime rc fixtures

These cases record behavior obtained once from Debian-patched procmail
3.23pre. The maintained test suite runs only `procmail-rs`; it does not build
or execute the reference program and does not access `external/`.

Each case contains the original `procmail.rc`, the corresponding
`procmail-rs.rc`, all runtime rc files, one input message, and three reviewed
results produced by the reference run:

- `expected.status` is the exact process exit status;
- `expected.destinations` records every selected destination;
- `expected.delivery` contains the exact bytes written by reference procmail.

Reference procmail in general-filter mode does not add an mbox postmark, while
the explicit `mbox:` backend always adds one. The maintained test therefore
requires a generated `From MAILER-DAEMON` postmark and compares every byte
after that line with `expected.delivery`. This is the only normalization made
by these fixtures.
