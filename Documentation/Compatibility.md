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

## Implemented compatibility

These behaviors follow procmail 3.22 semantics within the documented resource
ceilings and supported recipe subset.

| Area | Compatible behavior | Bounded implementation notes |
| --- | --- | --- |
| Reserved regex forms | Expands `^TO`, `^TO_`, `^FROM_DAEMON`, and `^FROM_MAILER` wherever they occur unless the caret is immediately preceded by `\\`. `^FROM_DAEMON` forces case-insensitive matching. | `LINEBUF` bounds only the user-written rc line. Generated macro text may exceed it but remains subject to the 64 KiB expanded-regex limit and the compiled-size limit. |
| `i` on a pipe | Ignores an error while writing the selected message bytes to the child. | Child status and filter-output validation remain separate, as they are not pipe-input write errors. |
| `r` on Maildir | Maildir delivery preserves the original message ending, so `r` has no additional effect. | procmail-rs preserves the same message bytes with or without `r`. |

## Supported rc subset

The accepted syntax is intentionally closed: syntax not listed here is either
rejected explicitly or is an ordinary user variable assignment.

| Syntax area | Supported forms |
| --- | --- |
| Statements | `NAME=value`, `INCLUDERC=expression`, `SWITCHRC=expression`, recipes, and nested recipe blocks |
| Variable references | `$NAME`, `${NAME}`, and `${NAME:-expression}` with bounded nesting; one outer pair of double quotes may surround an assignment value |
| Recipe header | `:0` followed by flags and an optional `: lockfile` |
| Condition source flags | default/`H` for normalized headers, `B` for body, and `HB` for their documented combined byte sequence |
| Recipe flags | `H`, `B`, `D`, `c`, `A`, `a`, `E`, `e`, `h`, `b`, `f`, `w`, `W`, `i`, and `r`, subject to action-specific checks |
| Conditions | Byte regex, leading `!` negation, `? shell command`, `< size`, `> size`, `H ?? regex`, `B ?? regex`, and `$NAME ?? regex` |
| Actions | Explicit Maildir or mbox delivery, trusted shell pipe action, `{ ... }` block, and the procmail-rs `headers { ... }` extension |
| Regex additions | Procmail `^TO`, `^TO_`, `^FROM_DAEMON`, `^FROM_MAILER`, `\<`, `\>`, `\+`, `\?`, `\|`, capture assignment with `\/`, and numbered `MATCH1` through the configured capture ceiling |
| Runtime files | Conditional and nested `INCLUDERC`; `SWITCHRC` abandons the current rc file after a successful switch |
| External values | Passwd-derived `HOME` and `LOGNAME`, system-derived `HOST`, read-only `PROCMAIL_VERSION`, and policy-checked `--set` values; ambient process variables are not imported |
| Logging | `LOGFILE`, `VERBOSE`, `LOGABSTRACT`, and `LOGDETAIL=values`; metadata mode omits sensitive values by default |
| Process settings | `SHELL`, `SHELLFLAGS`, `PATH`, `TIMEOUT`, `TRAP`, `EXITCODE`, and `UMASK` |
| Delivery settings | `MAILDIR`, `DURABILITY`, `LOCKMETHOD`, `LOCKFILE`, `LOCKEXT`, and `LOCKTIMEOUT` |

Shell command text is passed to the selected trusted shell. Shell parsing,
environment-prefix assignments, quoting, expansion, redirection, and pipelines
therefore follow that shell rather than an internal command tokenizer. Rc
variable expansion remains the limited syntax listed above and does not become
general shell evaluation.

## Native header editing extension

`headers { ... }` is a procmail-rs action for common header-only changes that
would otherwise require a pipe through `formail`. It is deliberately not
procmail syntax. A selected action applies every operation in source order and
then continues with the following recipe:

```text
:0
headers {
    remove X-Old-Status
    set X-Filter-Status: checked
    add X-Filter-Result: clean
    prepend X-Processed-By: procmail-rs
}
```

Field names are matched without regard to ASCII case. The operations behave as
follows:

| Operation | Behavior |
| --- | --- |
| `remove NAME` | Removes every field named `NAME`, including all continuation lines belonging to a folded field. |
| `set NAME: VALUE` | Replaces the first matching field at its existing position and removes later duplicates. Appends a new field when none exists. |
| `add NAME: VALUE` | Appends a new field even when fields with the same name already exist. Repeated additions retain source order. |
| `prepend NAME: VALUE` | Inserts a new field before every current field. A later `prepend` therefore appears before an earlier one. |

`NAME` must be non-empty printable ASCII without `:`. `VALUE` uses the bounded
rc expansion forms `$NAME`, `${NAME}`, and `${NAME:-expression}` when the
action executes. NUL, CR, LF, and requested folded continuations are rejected.
Inserted values are not reparsed as shell text. Existing fields and the body
remain byte-for-byte unchanged. New fields use the first physical header
line's LF or CRLF ending, or the separator's ending when the header is empty.

The condition and control flags `H`, `B`, `D`, `c`, `A`, `a`, `E`, and `e` are
accepted with their usual meanings. The action always continues after a
successful edit, so `c` does not change its behavior. Action flags `h`, `b`,
`f`, `w`, `W`, `i`, and `r`, as well as local lockfiles, are rejected because
the action neither invokes a child nor publishes a destination.

The complete edited header is checked against `LIMIT_MSG_SIZE`,
`LIMIT_MSG_HEADERS`, `LIMIT_HEADER_LINE`, and `LIMIT_HEADER_FIELD` before it
becomes visible. If expansion, validation, or a limit check fails, the earlier
message remains selected. Later conditions, runtime rc files, external
actions, and delivery see the edited header. A header-only path can still
stream the untouched body without retaining it.

This action is not a complete built-in replacement for `formail`. It does not
extract or rename fields, generate addresses or message identifiers, split
digests, rewrite the body, or implement other `formail` options. In particular,
the common `formail -I NAME:` removal idiom maps to `remove NAME`; `set NAME:`
creates an empty field. Unlike a `formail -I` filter, `set` keeps the position
of the first matching field. Use a trusted pipe action when broader `formail`
behavior is required.

Reserved procmail variables `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`, `LOG`,
`MSGPREFIX`, `NORESRETRY`, `PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`,
`SENDMAIL`, `SENDMAILFLAGS`, and `SHIFT` are rejected by name. Forward actions
beginning with `!` are also rejected. This makes unsupported behavior visible
instead of silently assigning it another meaning.

## Deliberate differences

| Area | procmail 3.22 | procmail-rs |
| --- | --- | --- |
| Native header editing | Requires an external filter such as `formail`; there is no `headers { ... }` action. | Provides the bounded `headers` extension described above. Rc files using it are intentionally not accepted by procmail 3.22. |
| Destination type and directory delivery | May infer a directory or mailbox from the current filesystem. | Never infers a backend from the filesystem. Requires `maildir:PATH` or a trailing `/` for a Maildir containing `tmp`, `new`, and `cur`, and requires `mbox:PATH` for mbox delivery. |
| Default delivery | Can fall back to `DEFAULT`, `ORGMAIL`, or the system mailbox. | Never selects an implicit destination. An undelivered original is an error. |
| Forwarding | A `!` action forwards through the configured sendmail command. | Rejected before message input; procmail-rs never forwards or invokes sendmail implicitly. |
| Comsat notification | `COMSAT` may enable notification after delivery. | `COMSAT` is rejected as an unsupported reserved variable; delivery has no notification side effect. |
| `HOST` | Initializes the variable from the current hostname, continues on an exact match, and ends the current rc file on a mismatch. | Implements the same control flow using the bounded UTF-8 node name returned by `uname`. Ambient environment values cannot replace it. Node names that are empty, invalid UTF-8, or longer than 255 bytes are rejected before message input. |
| Runtime rc files | Opens paths using the process filesystem permissions. | Requires trusted regular files owned by the current uid and rejects broadly writable files and symlinks. |
| Initial variables | Imports a broad process environment. | Gets `HOME` and `LOGNAME` from the current uid and accepts other external values only through `--set`. |
| `PROCMAIL_VERSION` | Contains the running procmail version number and cannot be changed. | Contains the bounded package version from `Cargo.toml` and cannot be changed. The value identifies procmail-rs and does not claim to be procmail 3.22. |
| Unsupported reserved variables | Variables such as `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`, `LOG`, `MSGPREFIX`, `NORESRETRY`, `PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`, `SENDMAIL`, `SENDMAILFLAGS`, and `SHIFT` retain their original special meanings. | Rejects these names explicitly in assignments, `--set`, and expansion references. Unknown names remain ordinary user variables. |
| `LOGABSTRACT` | Defaults to a final abstract containing `From`, `Subject`, destination, and message size; `no` suppresses it and `all` logs every successful delivery. | Accepts only the exact value `no`, including after bounded variable expansion. Abstract logging remains disabled because other modes could expose sensitive header values. A statically known unsupported value is rejected before message input; a runtime-derived value is rejected when its selected assignment executes. |
| Pipe command parsing | Uses a hybrid direct-command and shell parser. | Runs every trusted pipe command through the configured, policy-checked shell. |
| mbox in general-filter mode | A bare output file does not gain a generated postmark. | Explicit `mbox:` delivery always writes a complete mboxrd record with a generated postmark. |
| `i` on mbox or Maildir | May ignore a failed write and report success after a partial append or publish a truncated Maildir file. | Rejected before message input. Filesystem publication must complete successfully. |
| `i` on a recipe block | Ignored with a warning. | Ignored with a source-located warning; filesystem delivery still rejects `i`. |
| `r` on mbox | Raw file delivery suppresses the usual mailbox delimiter handling as well as final-newline normalization. | Retains the generated postmark and mboxrd quoting. It omits the normal blank record separator but adds one LF when needed so a following postmark starts on a new line. |
| `r` on a recipe block | Ignored with a warning. | Ignored with a source-located warning. |
| Local recipe lockfiles | Creates and later removes a named dotlock, or derives its name from the destination. | Defaults to a persistent, ownership-checked file held with `flock`. `LOCKMETHOD=dotlock` selects compatible creation, stale removal, and cleanup with the original pathname-replacement risk. |
| `LOCKEXT` | Defaults to `.lock` and is appended when deriving a local lockfile name. | Preserves the default and statement-order assignment. The suffix may be empty, is bounded to 4096 bytes, may not contain NUL or `/`, and the complete derived path remains bounded to 4096 bytes. |
| Implicit pipe lockfile | Attempts to derive a name from redirection found in the command. | Rejected before message input; shell command text is not reinterpreted to guess a lock path. |
| Lockfile on a recipe block | Documents that a lock on a non-forking block does not work as expected; procmail 3.22 was observed creating and removing the dotlock before the child sequence and logging `Extraneous locallockfile ignored`. | Requires an explicit lockfile name and holds the selected `flock` or dotlock across the complete child sequence. The path and active `LOCKMETHOD`, `LOCKTIMEOUT`, `UMASK`, variables, and `MAILDIR` are resolved when the block is selected. An implicit block lock is rejected because no single destination exists from which to derive it. |
| `LOCKFILE` | Replaces the preceding global dotlock and holds the new one until replacement or exit. | Preserves statement-order lifetime while using the active `LOCKMETHOD`; flock remains the default. |
| `LOCKTIMEOUT=0` | Waits indefinitely without stale-dotlock removal. | Rejected because all lock waits must remain finite. Values from 1 through 86400 seconds are accepted and also bound mbox flock waits. |
| `LINEBUF` | Defaults to 2048, has a minimum of 128, and may be changed while an rc file executes. Overflow may truncate data and set `PROCMAIL_OVERFLOW`. | Rejects overflow instead of truncating it, has a 1048576-byte ceiling, and accepts only literal top-level assignments because the complete typed recipe tree is built before message filtering. Mail input and trace limits remain separate. |
| `TIMEOUT=0` | Waits indefinitely for child termination. | Rejected because process waits must remain finite. The 960-second default and values from 1 through 86400 are supported. |
| `UMASK` | Changes the process umask and may permit group or other access when configured accordingly. | Accepts octal `0000` through `0777` in statement order, but only removes bits from restrictive backend modes. The process-wide umask is not changed and may remove more bits. |
| `TRAP` input | Runs on normal termination with the current message and appends one LF. | Matches this behavior after complete-input validation and supplies the final filtered message through bounded staging or the filter-owned buffer. It does not run after rejected partial input. |
| `TRAP` output and status | Sends stdout to the logging descriptor and can replace the result when `EXITCODE` is empty. | Sends both stdout and stderr to `LOGFILE`, applies bounded `TIMEOUT`, and preserves the recorded unset, empty, and explicit `EXITCODE` behavior. Start failure or timeout becomes status 75 only when `EXITCODE` is empty. |
| Timed-out descendants | Sends `SIGTERM` to the child selected by procmail's process tracking. | Runs each shell in a separate process group, then sends `SIGTERM` and `SIGKILL` to that group. A trusted command that deliberately leaves the group still requires external cgroup or namespace containment. |

## Updating this document

Add compatible behavior to the implemented table and add intentionally narrowed
or safer behavior to the differences table. Each entry needs focused tests that
exercise the procmail-rs behavior; where practical, store a reviewed reference
result without making the original executable a test dependency.
