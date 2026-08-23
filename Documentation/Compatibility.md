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
| `r` on Maildir | Maildir delivery already preserves the message ending, so `r` has no additional effect. | Preserves the message bytes with or without `r`. |
| `r` on mbox | Raw file delivery suppresses the usual mailbox delimiter handling as well as final-newline normalization. | Retains the generated postmark and mboxrd quoting. It omits the normal blank record separator but adds one LF when needed so a following postmark starts on a new line. |
| `r` on a recipe block | Ignored with a warning. | Rejected as unsupported instead of silently discarding the flag. |
| Local recipe lockfiles | Creates and later removes a named dotlock, or derives its name from the destination. | Defaults to a persistent, ownership-checked file held with `flock`. `LOCKMETHOD=dotlock` selects compatible creation, stale removal, and cleanup with the original pathname-replacement risk. |
| Implicit pipe lockfile | Attempts to derive a name from redirection found in the command. | Rejected before message input; shell command text is not reinterpreted to guess a lock path. |
| Lockfile on a recipe block | Holds the lock while the block executes. | Rejected before message input; local locks currently cover delivery and pipe actions only. |
| `LOCKFILE` | Replaces the preceding global dotlock and holds the new one until replacement or exit. | Preserves statement-order lifetime while using the active `LOCKMETHOD`; flock remains the default. |
| `LOCKTIMEOUT=0` | Waits indefinitely without stale-dotlock removal. | Rejected because all lock waits must remain finite. Values from 1 through 86400 seconds are accepted and also bound mbox flock waits. |

## Updating this document

Add an entry whenever compatibility is intentionally narrowed or behavior is
made safer than the reference implementation. A difference must have focused
tests that exercise the procmail-rs behavior; where practical, store a reviewed
reference result without making the original executable a test dependency.
