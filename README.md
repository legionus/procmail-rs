# procmail-rs

`procmail-rs` is an experimental mail filter for 64-bit Linux. The currently
implemented CLI can validate or explain an rc file and can filter one message
from standard input into explicitly selected Maildir destinations.

The program does not provide an implicit system mailbox or fallback delivery.

## Commands

```text
procmail-rs check   --config PATH [--set NAME=VALUE]...
procmail-rs explain --config PATH [--set NAME=VALUE]...
procmail-rs filter  --config PATH [--set NAME=VALUE]...
```

`check` validates the configuration without reading a message. `explain`
additionally prints a value-free description of the execution plan. `filter`
reads one message from standard input and attempts the selected deliveries.

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

All diagnostics go to standard error. Successful `check` and `filter`
operations are quiet. `explain` writes its requested plan description to
standard output.

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

## Mbox format

Mbox delivery is not implemented yet. Its selected on-disk format is
**mboxrd**, using the reversible quoting described by the
[Library of Congress format description](https://www.loc.gov/preservation/digital/formats/fdd/fdd000385.shtml).
The common record structure follows
[RFC 4155](https://www.rfc-editor.org/rfc/rfc4155.html), with the more specific
mboxrd quoting rules below.

The formatter will use these rules:

- Each record starts with exactly one ASCII postmark line beginning with
  `From ` and ending with LF. A later implementation step will define how its
  sender and timestamp are selected and how an incoming postmark is handled.
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
used. This mode coordinates cooperating local writers; no NFS-safety claim is
made.
