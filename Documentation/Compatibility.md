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
| Forwarding | A `!` action forwards through the configured sendmail command. | Rejected before message input; procmail-rs never forwards or invokes sendmail implicitly. |
| Generic directory delivery | A destination may select directory delivery after inspecting the filesystem. | Never inferred from the filesystem. Only `maildir:PATH` or the explicit trailing-slash Maildir syntax selects a directory backend, which must have `tmp`, `new`, and `cur`. |
| Comsat notification | `COMSAT` may enable notification after delivery. | `COMSAT` is rejected as an unsupported reserved variable; delivery has no notification side effect. |
| Non-empty `HOST` | Can stop processing when its hostname comparison succeeds. | Rejected before message input because hostname comparison is not implemented. An empty `HOST` remains available for failure handlers. |
| Runtime rc files | Opens paths using the process filesystem permissions. | Requires trusted regular files owned by the current uid and rejects broadly writable files and symlinks. |
| Initial variables | Imports a broad process environment. | Gets `HOME` and `LOGNAME` from the current uid and accepts other external values only through `--set`. |
| Unsupported reserved variables | Variables such as `DEFAULT`, `ORGMAIL`, `COMSAT`, `LOGABSTRACT`, `MSGPREFIX`, `NORESRETRY`, `SUSPEND`, `SENDMAIL`, `SENDMAILFLAGS`, and `SHIFT` retain their original special meanings. | Rejects these names explicitly in assignments, `--set`, and expansion references. Unknown names remain ordinary user variables. |
| Reserved regex forms | Expands `^TO`, `^TO_`, `^FROM_DAEMON`, and `^FROM_MAILER` wherever they occur unless the caret is immediately preceded by `\\`. `^FROM_DAEMON` forces case-insensitive matching. | Implements the same fixed byte-regex substitutions and case behavior. `LINEBUF` bounds only the user-written rc line; generated macro text may exceed it but remains subject to the 64 KiB expanded-regex limit and the compiled-size limit. |
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
| `LINEBUF` | Defaults to 2048, has a minimum of 128, and may be changed while an rc file executes. Overflow may truncate data and set `PROCMAIL_OVERFLOW`. | Rejects overflow instead of truncating it, has a 1048576-byte ceiling, and accepts only literal top-level assignments because the complete typed recipe tree is built before message filtering. Mail input and trace limits remain separate. |
| `TIMEOUT=0` | Waits indefinitely for child termination. | Rejected because process waits must remain finite. The 960-second default and values from 1 through 86400 are supported. |
| `UMASK` | Changes the process umask and may permit group or other access when configured accordingly. | Accepts octal `0000` through `0777` in statement order, but only removes bits from restrictive backend modes. The process-wide umask is not changed and may remove more bits. |
| `TRAP` input | Runs on normal termination with the current message and appends one LF. | Matches this behavior after complete-input validation and supplies the final filtered message through bounded staging or the filter-owned buffer. It does not run after rejected partial input. |
| `TRAP` output and status | Sends stdout to the logging descriptor and can replace the result when `EXITCODE` is empty. | Sends both stdout and stderr to `LOGFILE`, applies bounded `TIMEOUT`, and preserves the recorded unset, empty, and explicit `EXITCODE` behavior. Start failure or timeout becomes status 75 only when `EXITCODE` is empty. |
| Timed-out descendants | Sends `SIGTERM` to the child selected by procmail's process tracking. | Runs each shell in a separate process group, then sends `SIGTERM` and `SIGKILL` to that group. A trusted command that deliberately leaves the group still requires external cgroup or namespace containment. |

## Updating this document

Add an entry whenever compatibility is intentionally narrowed or behavior is
made safer than the reference implementation. A difference must have focused
tests that exercise the procmail-rs behavior; where practical, store a reviewed
reference result without making the original executable a test dependency.
