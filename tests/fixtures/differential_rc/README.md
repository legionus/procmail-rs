# Differential runtime rc fixtures

These cases record behavior obtained once from Debian-patched procmail
3.23pre. The maintained test suite runs only `procmail-rs`; it does not build
or execute the reference program and does not access `external/`.

Each case contains the original `procmail.rc`, the corresponding
`procmail-rs.rc`, all runtime rc files, one input message, and the reviewed
`expected.destinations` produced by the reference run. Destination files are
named so the test can compare the selected actions without comparing mbox
serialization details.

