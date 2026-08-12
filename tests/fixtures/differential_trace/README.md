# Differential trace fixtures

These fixtures retain filtering decisions observed with the bundled procmail
3.22 and compare them with typed events emitted by procmail-rs.  Normal test
runs do not compile or execute the original program.

`header_fallback/expected.events` was produced from procmail 3.22 verbose
output after removing the PID, timestamp, destination path, message metadata,
and regular-expression text.  The retained records correspond to:

```text
Assigning "BOX=selected"
No match on the first header condition
Match on the second header condition
```

It can be checked manually from the repository root with:

```sh
make -s -C external/procmail-3.22 CFLAGS0='-O -std=gnu89'
env -i HOME="$PWD" LOGNAME=user USER=user PATH=/usr/bin:/bin LC_ALL=C TZ=UTC \
    external/procmail-3.22/new/procmail -m \
    tests/fixtures/differential_trace/header_fallback/procmail.rc \
    < tests/fixtures/differential_trace/header_fallback/message.eml
```

The original configuration uses `/dev/null`, while the procmail-rs form uses
the explicit `mbox:/dev/null` destination syntax.  Destination spelling is
outside this fixture: it checks assignment order and header-filter decisions.
