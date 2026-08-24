# procmail-rs

`procmail-rs` is an experimental mail filter for 32-bit and 64-bit Linux. It
validates or explains an rc file and filters one message from standard input
into explicitly selected Maildir or mbox destinations.

The program does not provide an implicit system mailbox or fallback delivery.
See [Documentation/Compatibility.md](Documentation/Compatibility.md) for the
supported compatibility boundary and deliberate differences from procmail.

The security model assumes that messages, rc text, command-line values, paths,
filesystem state, and child-process output can all be malformed or hostile.
Input is bounded while it is read, arithmetic derived from it is checked, and
delivery is not published until a complete operation succeeds. Commands named
by an rc file are trusted code and are intentionally not sandboxed; use an
external namespace, cgroup, or service policy when they need containment.

## Build

The build requires the stable Rust toolchain and Cargo. The supported release
target is Linux with a 32-bit or 64-bit pointer width.

```text
cargo build --locked --release
./target/release/procmail-rs --version
```

Development and release checks are described in
[Documentation/ReleasePolicy.md](Documentation/ReleasePolicy.md).
The project is distributed under the MIT license; dependency and fixture
provenance is recorded in [Documentation/Licenses.md](Documentation/Licenses.md).

## Minimal configuration

Create the Maildir structure before filtering; delivery never creates or
repairs its `tmp`, `new`, or `cur` directories.

```text
MAILDIR=/home/user/Mail
LIMIT_MSG_SIZE=64M

:0
inbox/
```

Then validate it and filter one message:

```text
procmail-rs check --config ./procmailrc
procmail-rs filter --config ./procmailrc <message.eml
```

See [Documentation/Limits.md](Documentation/Limits.md) for resource settings
and [Documentation/Delivery.md](Documentation/Delivery.md) for destination,
locking, and durability behavior.

## Commands

```text
procmail-rs check   --config PATH [--set NAME=VALUE]...
procmail-rs explain --config PATH [--set NAME=VALUE]...
procmail-rs filter  --config PATH [--set NAME=VALUE]...
```

`check` validates the configuration without reading a message. `explain`
additionally prints a value-free description of the execution plan. `filter`
reads one message from standard input and attempts the selected deliveries.

`check` recursively opens and validates every `INCLUDERC` and `SWITCHRC` path
that can be computed from command-line values and unconditional rc
assignments. A path that depends on message processing, such as `$MATCH` or
`$LASTFOLDER`, produces a warning because no message is read. Warning output is
bounded and does not contain the unresolved expression or computed path.
Successful checking therefore confirms only the root file and runtime files
whose paths were available at check time; message-selected files are validated
when `filter` reaches them.

## Exit statuses

Exit statuses are part of the CLI interface. Values from `sysexits` are used
when their established meaning fits. Status 79 is specific to `procmail-rs`.

| Status | Name | Meaning | Recommended caller action |
| ---: | --- | --- | --- |
| 0 | `EX_OK` | The configuration check, explanation, or complete filtering operation succeeded. | Accept the result and do not retry. |
| 65 | `EX_DATAERR` | The input message was rejected, for example because a configured message limit was exceeded. | Do not retry the same bytes unchanged. Quarantine or reject the message according to local policy. |
| 70 | `EX_SOFTWARE` | An internal failure prevented a reliable result. | Keep the message and retry later. Alert an operator if the failure repeats. |
| 73 | `EX_CANTCREAT` | A permanent destination problem prevented delivery, such as a missing, invalid, or inaccessible Maildir. | Do not retry until the destination or configuration changes. Keep, quarantine, or bounce the message according to local policy. |
| 75 | `EX_TEMPFAIL` | A temporary resource or delivery failure occurred. | Keep the message queued and retry later. |
| 78 | `EX_CONFIG` | The rc file or command-line configuration is invalid. Configuration is rejected before stdin is read. | Fix the configuration. An MTA should defer queued mail rather than discard it while the configuration is broken. |
| 79 | `PROCMAIL_RS_UNDELIVERED` | No final recipe delivered the original message. Copy recipes may already have published copies. | Apply an explicit fallback, quarantine, or rejection policy. Never interpret this status as successful delivery. |

All diagnostics go to standard error. Successful `check` operations without
dynamic-path warnings and successful `filter` operations are quiet. `explain`
writes its requested plan description to standard output.

### MTA integration

The invoking MTA must retain ownership of the message unless `filter` exits
with status 0. In particular, it must not discard a message merely because
one copy destination was published before a later operation failed.

A wrapper can group statuses by local queue policy while preserving the exact
status in logs. For example:

```sh
procmail-rs filter --config /etc/procmail-rs.rc
status=$?

case "$status" in
0)
	exit 0
	;;
70|75|78)
	# Ask the MTA to retain the message and try again later.
	exit 75
	;;
65|73|79)
	# Hand the message to the site's reject, bounce, or quarantine policy.
	exit "$status"
	;;
*)
	# Unknown results must not be treated as successful delivery.
	exit 70
	;;
esac
```

Retrying after a multi-destination or copy delivery failed can publish a
duplicate at destinations that succeeded before the failure. A production
integration should account for that possibility when it uses copy recipes.

## Runtime rc files

`INCLUDERC` and `SWITCHRC` select files while a message is being evaluated.
Unlike procmail 3.22, `procmail-rs` applies a trust policy before parsing such
a file:

- the final path component is opened with `O_NOFOLLOW` and must be a regular
  file;
- its numeric owner must match the owner of the root rc file;
- group and other users must not have write permission;
- the type, owner, and mode are checked on the opened descriptor rather than
  through a separate pathname lookup.

The root rc file is selected by the command line and establishes the trusted
numeric owner. It is not subjected to the runtime ownership and mode checks.
Administrators must protect that file and the command line that selects it.

Only a symlink in the final path component is rejected. Intermediate directory
symlinks and hard links to an otherwise accepted file are allowed. Directory
changes may alter which file is opened, but the opened file must still pass
the type, owner, and mode checks above.

Original procmail performs none of these ownership or permission checks. A
configuration that includes a group-writable file, a file owned by another
user, or a final symlink will therefore be rejected by `procmail-rs`. A failed
runtime include or switch is diagnosed and follows the documented procmail
recovery behavior; resource-limit failures remain fatal.

## Message input and staging

`filter` first reads only the bounded header section. Runtime assignments,
`INCLUDERC`, `SWITCHRC`, and header conditions execute while standard input
remains positioned at the body. If this produces a final header-only delivery
plan, the body is streamed directly from standard input to the selected sinks
without creating a staging file.

An executed body or whole-message condition, a final-size test, mbox delivery,
a reachable `TRAP`, or another order-dependent action requires a replayable
message. In that case the complete bounded input is written to private staging
under the `MAILDIR` active at the point where evaluation is deferred. A runtime
rc file that is never selected cannot by itself trigger staging. At completion,
one bounded view represents either the mapped staged message or the owned
result of the last successful filter, without copying either one.

When a copy destination is selected before a later rule needs the body, the
copy and staging file receive the same input pass but remain private until the
body has been read and validated. An input-limit or write failure therefore
cannot publish that early copy.

## External command timeout

`TIMEOUT` defaults to 960 seconds and accepts decimal values from 1 through
86400. It applies in statement order to pipe actions, filters, program
conditions, and `TRAP`. Zero is rejected because it requests an unbounded wait.

Each shell runs in a separate process group. Timeout supervision remains
active while procmail-rs writes stdin, reads filter stdout, and waits for the
direct shell. On expiration it sends `SIGTERM` to the group, waits 250 ms,
sends `SIGKILL`, and reaps the direct shell. The result then follows the usual
status flags: no `w` or `W` ignores it, `w` reports it, `W` suppresses that
diagnostic, `i` affects only stdin write errors, and an observed failure can
select an `e` recipe.

A trusted command can deliberately leave its process group. Such a process is
outside this timeout mechanism and requires an external cgroup, namespace, or
service-manager policy when containment is required.

## Exit trap

`TRAP` stores a bounded trusted-shell command. The last executed non-empty
assignment runs after recipe processing and complete-input validation; an
empty assignment disables it. `check` and `explain` report shell use but never
execute the command. No trap runs for a configuration error, incomplete or
rejected message input, or termination by a signal.

The command receives the final message after successful filters and one
additional LF, matching recorded procmail behavior. Its stdout and stderr are
both appended to `LOGFILE`, or both inherit procmail-rs stderr when no log is
selected. The command uses the active bounded environment, shell settings,
`UMASK`, and `TIMEOUT` process-group supervision.

When `EXITCODE` was never assigned, its provisional filtering status is made
available to the command and the trap status does not replace it. An explicit
non-empty `EXITCODE` also remains authoritative. With `EXITCODE=""`, a nonzero
trap status becomes the final status; a successful trap leaves the filtering
result unchanged, while failure to start or a timeout selects temporary
failure status 75.

## Mbox format

Mbox delivery writes the **mboxrd** on-disk format, using the reversible
quoting described by the
[Library of Congress format description](https://www.loc.gov/preservation/digital/formats/fdd/fdd000385.shtml).
The common record structure follows
[RFC 4155](https://www.rfc-editor.org/rfc/rfc4155.html), with the more specific
mboxrd quoting rules below.

The formatter will use these rules:

- Each record starts with exactly one generated ASCII postmark line using
  sender `MAILER-DAEMON`, the current UTC timestamp in ctime shape, and LF.
  An incoming leading `From ` line remains message data and is quoted like any
  other line; hostile input cannot select envelope metadata.
- The message is treated as bytes. Existing line endings are not normalized.
- At the start of every physical message line, including lines in the header
  section, a sequence matching zero or more `>` bytes followed by `From ` is
  prefixed with one additional `>`. For example, `From ` becomes `>From ` and
  `>>From ` becomes `>>>From `.
- An mboxrd reader reverses that transformation by removing exactly one `>`
  from every message line matching one or more `>` bytes followed by `From `.
- Every record ends with a blank LF-terminated separator line. If necessary,
  the formatter adds a final LF to the message and then one separator LF; it
  does not change the in-memory message.
- Existing `Content-Length:` fields are preserved as message data but ignored
  for framing. The formatter does not generate or update them. Consumers must
  open the result as mboxrd rather than mboxcl or mboxcl2.

This deliberately differs from the ordinary Berkeley-style escaping in the
procmail 3.22 reference source, which quotes an unquoted `From ` line but does
not provide reversible quoting for every existing `>From ` line. Mailboxes
written in different mbox variants must not be mixed.

Local mbox writers use a kernel `flock` exclusive lock with bounded retries.
Mailbox path components and the final file are opened without following
symlinks, and a mailbox with multiple hard links is rejected. Dotlock is not
used. Existing files must be regular and writable by the process; ownership is
not changed or required to match the effective user, so group-authorized local
mailboxes remain usable. New files request mode `0600`, with ambient umask only
able to remove permissions. This mode coordinates cooperating local writers;
no NFS-safety claim is made.

Mbox delivery stages the bounded input under `MAILDIR`, then holds the kernel
lock while it records the original length, appends one complete record, and
performs the selected `DURABILITY` operations. A write or sync failure attempts
to truncate the mailbox back to its original length before unlocking. Failure
of that recovery is reported as an internal error. `LASTFOLDER` changes only
after a successful append.

Local lockfiles on delivery and pipe recipes use `LOCKMETHOD=flock` by default. The lock path is
opened without following symlinks, must be a regular file owned by the current
uid with one link and no group or other permissions, and remains present after
the recipe; closing its descriptor releases the kernel lock. A bare trailing
recipe colon derives the name by appending `.lock` to the resolved filesystem
destination. An implicit lock is rejected for pipe actions.

`LOCKMETHOD=dotlock` selects procmail-compatible named-file locking. The
read-only file exists only while the selected action runs, retries every eight
seconds, and is treated as stale after 1024 seconds. This mode intentionally
inherits procmail's pathname race: another process can replace the entry
between inspection and removal. Use it only when coordination with software
that observes dotlock creation and deletion is more important than protection
against hostile same-directory changes. `LOCKMETHOD` takes effect in rc
statement order.

Local lockfiles on recipe blocks remain unsupported and are rejected before
message input.

`LOCKFILE=PATH` holds a global lock from that assignment until another
`LOCKFILE` assignment replaces it, an empty assignment releases it, or the
process exits. The path uses the same bounded expansion and `MAILDIR`-relative
resolution as recipe lockfiles. Global locks use the active `LOCKMETHOD`.

`LOCKTIMEOUT` controls global locks, recipe locks, and the kernel lock taken
while appending to an mbox. It defaults to 1024 seconds and accepts decimal
values from 1 through 86400. Unlike original procmail, zero is rejected
because it requests an unbounded wait. Changes apply only to following lock
attempts; they do not shorten a wait that has already begun.

`UMASK` defaults to octal `077` and accepts octal values from `0000` through
`0777`. It applies in rc statement order to newly created Maildir messages,
mbox files, local lockfiles, and `LOGFILE`. The value is combined with each
backend's restrictive requested mode, so it can remove owner permissions but
cannot grant group or other access. Existing files and private staging files
are not chmodded. procmail-rs does not change the process-wide umask; the
ambient process umask may therefore remove additional permissions.

`LINEBUF` defaults to 2048 bytes and accepts literal decimal values from 128
through 1048576. It bounds each following physical rc line, a continued pipe
command as a whole, and values produced by procmail-rs expansion. Existing
smaller path, command, assignment, and regex ceilings still apply. It does not
limit message header lines or trace records, which retain their independent
limits. Because procmail-rs builds its typed recipe tree before filtering,
`LINEBUF` is rejected inside recipe blocks and cannot use variable expansion.
