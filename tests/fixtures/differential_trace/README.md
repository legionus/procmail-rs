# Differential trace fixtures

These fixtures retain filtering decisions generated once with Debian-patched
procmail 3.23pre and compare them with typed events emitted by procmail-rs.
The repository neither builds nor executes the reference program.

`header_fallback/expected.events` was produced from reference procmail verbose
output after removing the PID, timestamp, destination path, message metadata,
and regular-expression text.  The retained records correspond to:

```text
Assigning "BOX=selected"
No match on the first header condition
Match on the second header condition
```

`header_fallback/procmail.rc` is the exact configuration used for that
one-time reference run. Ordinary tests execute only `procmail-rs.rc`; keeping
both files allows reviewers to verify that the stored events came from an
equivalent scenario.

Destination spelling is outside this fixture: it checks assignment order and
header-filter decisions.
