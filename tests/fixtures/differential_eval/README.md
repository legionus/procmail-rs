# Differential evaluation fixtures

These cases compare behavior implemented in Milestone 7 with reviewed results
generated once with Debian-patched procmail 3.23pre. The expected results are
now ordinary fixtures: this repository neither builds nor executes the
reference program.

Each case keeps the complete comparison record:

- `procmail.rc` is the exact reference configuration used once to obtain the
  expected result;
- `message.eml` is the input given to both implementations;
- `expected.destinations` is the reviewed reference result;
- `expected.outcome` records whether the original was delivered and the exact
  number of successful actions;
- `procmail-rs.rc` expresses the same scenario using explicit destination
  syntax accepted by this project.

Ordinary tests evaluate only `procmail-rs.rc` and require both stored result
files. They compare selected destination basenames and the exact final
evaluation outcome. They never execute `procmail.rc`; that file remains beside
the result so reviewers can verify that both configurations describe the same
filtering behavior.

Do not regenerate expected results during a test run. If support is expanded,
obtain and review the new reference behavior separately, then commit the
paired configurations, input, and stored result.
