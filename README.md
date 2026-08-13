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
