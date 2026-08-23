<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Procmail compatibility

`procmail-rs` supports a deliberately limited procmail rc language. It is not
a drop-in local delivery agent, and compatibility never permits partial
delivery, implicit mailbox selection, unbounded input, or ambient environment
imports.

An unsupported construct is rejected with its rc source line before stdin is
read whenever it can be identified during configuration loading. Runtime rc
files are checked when their `INCLUDERC` or `SWITCHRC` statement executes.

## Deliberate differences

| Area | procmail 3.22 | procmail-rs |
| --- | --- | --- |
| Destination type | May infer a directory or mailbox from the current filesystem. | Requires `maildir:PATH`, a trailing `/`, or `mbox:PATH`. |
| Default delivery | Can fall back to `DEFAULT`, `ORGMAIL`, or the system mailbox. | Never selects an implicit destination. An undelivered original is an error. |
| Runtime rc files | Opens paths using the process filesystem permissions. | Requires trusted regular files owned by the current uid and rejects broadly writable files and symlinks. |
| Initial variables | Imports a broad process environment. | Gets `HOME` and `LOGNAME` from the current uid and accepts other external values only through `--set`. |
| Pipe command parsing | Uses a hybrid direct-command and shell parser. | Runs every trusted pipe command through the configured, policy-checked shell. |
| mbox in general-filter mode | A bare output file does not gain a generated postmark. | Explicit `mbox:` delivery always writes a complete mboxrd record with a generated postmark. |
| `i` on a pipe | Ignores an error while writing the selected message bytes to the child. | Supported with the same limited purpose. Child status and filter-output validation remain separate. |
| `i` on mbox or Maildir | May ignore a failed write and report success after a partial append or publish a truncated Maildir file. | Rejected before message input. Filesystem publication must complete successfully. |
| `i` on a recipe block | Ignored with a warning. | Rejected as unsupported instead of silently discarding the flag. |

## Updating this document

Add an entry whenever compatibility is intentionally narrowed or behavior is
made safer than the reference implementation. A difference must have focused
tests that exercise the procmail-rs behavior; where practical, store a reviewed
reference result without making the original executable a test dependency.
