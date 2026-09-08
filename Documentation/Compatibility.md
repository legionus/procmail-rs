<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Procmail compatibility

`procmail-rs` supports a deliberately limited procmail rc language. It is not
a drop-in local delivery agent, and compatibility never permits partial
delivery, implicit mailbox selection, unbounded input, or ambient environment
imports.

An unsupported construct is intended to be rejected with its rc source line
before stdin is read whenever it can be identified during configuration
loading. Runtime rc files are checked when their `INCLUDERC` or `SWITCHRC`
statement executes. The audit below identifies remaining syntax that can still
be accepted with a different meaning; until those cases are fixed, a successful
`check` does not by itself establish compatibility with procmail 3.22.

## Implemented compatibility

These behaviors follow procmail 3.22 semantics within the documented resource
ceilings and supported recipe subset.

| Area | Compatible behavior | Bounded implementation notes |
| --- | --- | --- |
| Reserved regex forms | Expands `^TO`, `^TO_`, `^FROM_DAEMON`, and `^FROM_MAILER` wherever they occur unless the caret is immediately preceded by `\\`. `^FROM_DAEMON` forces case-insensitive matching. | `LINEBUF` bounds only the user-written rc line. Generated macro text may exceed it but remains subject to the 64 KiB expanded-regex limit and the compiled-size limit. |
| `i` on a pipe | Ignores an error while writing the selected message bytes to the child. | Child status and filter-output validation remain separate, as they are not pipe-input write errors. |
| `r` on Maildir | Maildir delivery preserves the original message ending, so `r` has no additional effect. | procmail-rs preserves the same message bytes with or without `r`. |

## Supported rc subset

The intended accepted syntax is closed. The reference audit below records the
remaining places where syntax outside this table is not yet distinguished from
a supported regex, assignment value, or destination.

| Syntax area | Supported forms |
| --- | --- |
| Statements | `NAME=value`, assignments containing backquoted commands, `INCLUDERC=expression`, `SWITCHRC=expression`, recipes, and nested recipe blocks |
| Variable references | `$NAME`, `${NAME}`, `${NAME-word}`, `${NAME:-word}`, `${NAME+word}`, and `${NAME:+word}` with bounded nesting and lazy selection; assignment values may concatenate unquoted, single-quoted literal, and double-quoted fragments into one word |
| Recipe header | `:0` followed by flags and an optional `: lockfile` |
| Condition source flags | default/`H` for normalized headers, `B` for body, and `HB` for their documented combined byte sequence |
| Recipe flags | `H`, `B`, `D`, `c`, `A`, `a`, `E`, `e`, `h`, `b`, `f`, `w`, `W`, `i`, and `r`, subject to action-specific checks |
| Conditions | Byte regex, leading `!` negation, shell-expanded `$` conditions, `? shell command`, `< size`, `> size`, `H ?? regex`, `B ?? regex`, and `$NAME ?? regex` |
| Actions | Explicit Maildir or mbox delivery, including bounded backquoted destination commands; explicit discard through an unmarked `/dev/null`; trusted shell pipe action; a sole `|` for stdout delivery; command-output capture with `NAME=| command`; `{ ... }` block; and the procmail-rs `headers { ... }` extension |
| Regex dialect | One byte-oriented Rust `regex-syntax` dialect with counted repetition, named ASCII classes, and the procmail `^TO`, `^TO_`, `^FROM_DAEMON`, `^FROM_MAILER`, `\<`, `\>`, `^^`, and `\/` extensions. Capturing groups populate numbered `MATCH1` values through the configured capture ceiling. |
| Runtime files | Conditional and nested `INCLUDERC`; `SWITCHRC` abandons the current rc file after a successful switch |
| External values | Passwd-derived `HOME` and `LOGNAME`, system-derived `HOST`, read-only `PROCMAIL_VERSION`, and policy-checked `--set` values; ambient process variables are not imported |
| Logging | `LOGFILE`, `VERBOSE`, `LOGABSTRACT`, and `LOGDETAIL=values`; metadata mode omits sensitive values by default |
| Process settings | `SHELL`, `SHELLFLAGS`, `PATH`, `TIMEOUT`, `TRAP`, `EXITCODE`, and `UMASK` |
| Delivery settings | `MAILDIR`, `DURABILITY`, `LOCKMETHOD`, `LOCKFILE`, `LOCKEXT`, `LOCKSLEEP`, and `LOCKTIMEOUT` |

## Reference audit

This section compares the accepted language with `procmailrc(5)`, the
practical recipes in `procmailex(5)`, the scoring rules in `procmailsc(5)`,
and the corresponding procmail 3.22 parser and evaluator. `external/` is only
a review source; installed procmail-rs tests do not depend on it.

### Rc language

| Documented procmail behavior | Current status | Compatibility consequence |
| --- | --- | --- |
| Ordinary regex, `!`, `?`, `<`, `>`, and `NAME ??` conditions | Supported within the limits described above. | Program conditions use the configured trusted shell and finite `TIMEOUT`. |
| A condition beginning with `$` is expanded using shell substitution rules inside double quotes and then reparsed as a condition. | Supported with the project's bounded variable syntax, procmail's `$\NAME` regex quoting, and backquoted commands. | Intermediate text is limited by active `LINEBUF`; commands receive the complete current message and use active `TIMEOUT` and `LOGFILE`. Unsupported special parameters are rejected explicitly. Runtime-dependent forms conservatively require complete staging because the resulting condition type is not known before evaluation. |
| `w^x` weighted regex, program, and length conditions; final score in `$=` | Explicitly rejected as one unsupported condition category. | Implement all scoring forms together with bounded match counting, checked numeric handling, and `$=` so a mixed recipe cannot receive partial scoring behavior. |
| A trailing backslash continues a condition; shell-expanded conditions retain continuation whitespace. | Supported. Each physical line and the complete joined condition are bounded by the active `LINEBUF`. | Leading space and tab are removed from continued ordinary conditions and retained in `$` conditions before their expansion pass. |
| Procmail ERE operators and its `^`, `$`, `^^`, `\<`, `\>`, and `\/` extensions | Partly supported by parsing the source with upstream `regex-syntax`, locating extensions through typed AST nodes and source spans, applying bounded replacements, and compiling with the Rust byte-regex engine. | procmail-rs deliberately uses the richer Rust regex dialect. Counted repetition uses `{m,n}` and literal braces must be escaped, while original procmail treats braces as ordinary text. Named ASCII classes and other documented Rust forms are accepted. Original procmail selects the leftmost shortest span before `\/` and the leftmost longest suffix after it; the Rust engine is leftmost-first. This difference is observable when alternatives are ambiguous, as recorded in `tests/fixtures/regex_match_selection/reference-behavior.md`. |
| `\/` inside groups, alternatives, and expressions containing several markers | Supported with at most 64 markers per expression. Focused tests preserve results recorded with Debian-patched procmail 3.23pre. | Nested markers include the complete matched suffix and a terminal LF consumed by `$`. A successful alternative which does not reach a marker retains the old `MATCH`; if a successful path reaches several markers, the last reached marker selects its value. |
| Shell-style assignments, including single and double quotes, escapes, unsetting with bare `NAME`, field splitting, and all documented parameter forms | One shell-like assignment word is supported. Unquoted, single-quoted, and double-quoted fragments may be concatenated; single quotes suppress expansion, commands, and backslash processing. A `#` begins a comment only after whitespace at a word boundary. Byte length, shell-pattern prefix or suffix removal, and ASCII case conversion are supported. The `:=` and `:?` forms are privacy-preserving procmail-rs extensions: `:=` transactionally changes only user variables, while `:?` never evaluates or reveals its diagnostic word. | A second unquoted word and unterminated quotes are rejected. Bare-name unsetting and field splitting of command output remain absent. Pattern matching is byte-oriented and subject to a fixed work ceiling. Case conversion is locale-independent and preserves non-ASCII bytes. `:=` is accepted only in assignment values and destination expressions; other contexts reject it before message input. |
| Backquoted commands in assignments and in mailbox names | Supported. Each command receives the complete current message and its stdout participates in bounded path construction. | Destination output must be UTF-8 and obeys both active `LINEBUF` and the fixed path ceiling. It is resolved only after every fragment succeeds. |
| `| command`, `NAME=| command`, and a sole `|` that writes the selected input to stdout | All three explicit recipe forms are supported. | A sole `|` writes the area selected by `h`/`b` directly, applies `r` and `i`, and does not start a shell. `DEFAULT=|` remains outside project scope because implicit fallback delivery is absent. |
| A pipe without `w` or `W` may continue without waiting after its input has been accepted. | procmail-rs supervises and reaps every shell even when its normal exit status is ignored. | Side-effect timing and lock lifetime differ. Preserving process supervision is safer; compatibility may require a documented asynchronous mode rather than weakening the default silently. |
| `c` on a nesting block clones processing and lets the parent skip the block. | Explicitly rejected. | Supporting it requires two bounded execution branches and clear publication/error ordering; it must not be approximated as an ordinary block. |
| `h` or `b` on file delivery writes only the selected part and may discard the other part. | Explicitly rejected for filesystem delivery. | This is a deliberate data-loss prevention measure. Keep it as an explicit difference unless partial-message delivery becomes an opt-in feature. |
| Mailbox actions may contain several directory destinations, ordinary directory folders, MH folders ending in `/.`, or Maildir folders ending in `/`. | Only one mbox or Maildir target is accepted; ordinary directory folders, MH folders, and multi-folder hardlink delivery are absent. | Whitespace in an unmarked destination is rejected both before and after variable expansion. Explicit `mbox:` and `maildir:` paths may contain whitespace because their backend and single-target meaning are unambiguous. |
| Existing directories can select directory delivery even without a suffix. | Deliberately not inferred from filesystem state. | Use `maildir:PATH` or a trailing `/`; every other bare path deterministically selects mbox. |
| `INCLUDERC` and `SWITCHRC` execute in statement order; empty `SWITCHRC` ends the current rc file; `/dev/null` is a valid switch target. | Ordered include, switch, empty switch, and a resolved `SWITCHRC=/dev/null` are supported with bounded runtime loading. | The null switch counts toward the transition limit and ends the current rc file without opening the device. `INCLUDERC=/dev/null` remains subject to the regular-file policy. |
| Old `:n` recipe headers and unlimited nesting | Only `:0` and bounded nesting are accepted. | Both differences fail explicitly and do not risk a different delivery. |

### Documented variables

| Status | Variables | Notes |
| --- | --- | --- |
| Supported or intentionally narrowed | `HOME`, `LOGNAME`, `PATH`, `SHELL`, `SHELLFLAGS`, `MAILDIR`, `LOGFILE`, `VERBOSE`, `LOGABSTRACT`, `LOCKFILE`, `LOCKEXT`, `LOCKSLEEP`, `LOCKTIMEOUT`, `TIMEOUT`, `HOST`, `UMASK`, `TRAP`, `EXITCODE`, `LASTFOLDER`, `MATCH`, `INCLUDERC`, `SWITCHRC`, `PROCMAIL_VERSION`, and `LINEBUF` | Exact restrictions are recorded in this document and in the limits documentation. `MATCH1`, `MATCH2`, and later numbered captures are procmail-rs additions. |
| Explicitly rejected | `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`, `DROPPRIVS`, `LOG`, `MSGPREFIX`, `NORESRETRY`, `PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`, `SENDMAIL`, `SENDMAILFLAGS`, `SHIFT`, and `LIMIT_RC_SIZE` | These names cannot accidentally act as ordinary variables. Implementing `DROPPRIVS` is outside project scope. `LIMIT_RC_SIZE` cannot safely configure the read which has already consumed its own assignment. |
| Original startup environment behavior | `IFS`, `ENV`, and `PWD` are cleared or preset, and other ambient variables are generally imported. | procmail-rs instead builds a bounded child environment from its runtime variable table. This is deliberate, but rc assignments with these names remain ordinary exported variables. |

### Coverage of `procmailex(5)` patterns

The common examples using regex selection, mbox delivery, Maildir delivery,
copy recipes, `A`/`a`/`E`/`e`, program conditions, external filters,
command-output assignments, destination command substitution, pipe-to-stdout,
shell-expanded conditions, `MATCH`, `TRAP`, and `EXITCODE` have corresponding
implementation paths. The manual's forwarding and autoreply examples remain
outside project scope. Its scoring, directory-folder, MH, multi-folder,
and block-copy examples expose the gaps listed above.

The stored differential fixtures cover every supported recipe flag, condition
search areas, folded and malformed header lines, waited and quiet child
failures, literal shell-expanded text, and unambiguous `MATCH` spans. They do
not yet constitute a systematic example-by-example suite for all supported
manual constructs or compare captures across ambiguous expressions.

### Compatibility improvement order

1. Treat the documented Rust regex dialect as a deliberate migration
   difference. Matching original procmail's input-dependent shortest/longest
   selection would require a regex engine with those semantics; AST-level
   alternative reordering or greediness changes are not sufficient.
2. Decide whether weighted scoring is needed by real migration rc files. If it
   is, implement all three scoring categories and `$=` together; partial
   scoring support would make mixed recipes misleading.
3. Consider ordinary directory folders, MH folders, and multi-folder delivery
   only after their naming, locking, rollback, hardlink, and partial-publication
   behavior has dedicated tests. Do not recover compatibility by inspecting a
   bare path and choosing a backend from mutable filesystem metadata.

Shell command text is passed to the selected trusted shell. Shell parsing,
environment-prefix assignments, quoting, expansion, redirection, and pipelines
therefore follow that shell rather than an internal command tokenizer. Rc
variable expansion remains the limited syntax listed above and does not become
general shell evaluation.

## Shell-expanded conditions

A condition beginning with `$` expands its remainder using double-quoted
shell-like rules and reparses the bounded result as a condition:

```text
:0
* $^To:.*<$\LOGNAME>
maildir:addressed/
```

The supported parameter forms use the runtime values
active when the condition is reached. Procmail's `$\NAME` form inserts the
value as a literal regex fragment, including a leading empty noncapturing group
that prevents the value from becoming `!`, `$`, `?`, a size test, scoring text,
or a special search-area prefix. Inserted values are not scanned again during
that expansion pass. A complete expanded result beginning with `$` starts a
new bounded expansion-and-reparse pass, up to the shared expansion-depth
ceiling.

Every intermediate result is limited by active `LINEBUF`. The reparsed result
must be one of the supported condition forms and remains subject to the regex
length, compiled-size, capture, and program-command limits. Statically known
results are parsed and regex-compiled before stdin is read. A condition using
`MATCH`, `LASTFOLDER`, command output, or another runtime-produced value is
resolved only when evaluation reaches it and conservatively requires the
complete message because it may become a body regex, size test, or program
condition.

Backquoted commands receive the complete current message on stdin when the
condition is reached. Their bounded stdout has all trailing newlines removed,
uses the active `TIMEOUT` and `LOGFILE`, and then participates in the same
reparse pass as literal and variable parts. Unsupported special parameters and
physical condition continuations are bounded before expansion. Ordinary
substitution requires UTF-8 runtime data; `$\NAME` can safely quote arbitrary
bytes into an ASCII byte-regex fragment.

## Command output assignments

Two procmail-compatible forms assign the stdout of a trusted shell command to
an rc variable. They execute only during `filter`; `check` and `explain`
validate and report the presence of shell execution without running it or
showing command text or assigned values.

A backquoted command may appear among literal and expanded fragments in an
ordinary assignment:

```text
LABEL="prefix-`printf '%s\n' "$MATCH1"`-suffix"
```

Each substitution receives the complete current message independently on
stdin. All trailing LF bytes are removed from its stdout before the next
fragment is appended. A normal nonzero child status does not suppress the
captured bytes. Every fragment is assembled privately, and the variable is
changed only after all commands and expansions succeed. Reaching this form
requires complete replayable input and therefore private staging under the
active `MAILDIR`.

A recipe action captures stdout with different newline and status behavior:

```text
:0 hW
FIELD=| extract-field
```

The `h` flag selects the header section including its terminating empty line,
`b` selects only the body, and neither flag selects the complete current
message. Without `r`, one LF is appended when header or complete-message input
lacks a final LF. Body-only input receives one LF unless it already ends in two
LF bytes, matching original procmail's representation of that selected area;
`r` preserves the selected ending. Header-only capture can execute before the
body is read, while body and complete-message capture require staging. Exactly
one trailing LF is removed from successful stdout.

Without `w` or `W`, a normal nonzero child status is ignored. With either flag
it makes the action fail and preserves the variable's previous value; `W`
only suppresses the child-failure diagnostic. The `i` flag ignores only a
failure while writing the selected input. A failed capture can select a
following `e` recipe, while a successful one can select `a` or `A`, and the
recipe sequence continues after a successful assignment.

For both forms, stdout is read concurrently with stdin and is bounded before
allocation grows past the active limit. The raw captured output, before LF
removal, may not exceed the smaller of the active `LINEBUF` and the fixed
ceiling for the assigned variable. Exceeding that limit terminates the
process group and fails the assignment. `TIMEOUT` also covers input, output,
and child termination; a timeout always fails a command-output assignment,
even without `w` or `W`. Stderr is appended to `LOGFILE`, or inherits
procmail-rs stderr when no log is selected.

Commands run as `SHELL SHELLFLAGS command`, where `SHELLFLAGS` is passed as one
argument. The child environment is rebuilt only from bounded runtime rc
variables and the defaults `SHELL=/bin/sh`, `SHELLFLAGS=-c`, and
`PATH=/usr/bin:/bin`; the ambient process environment is not inherited. The
shell path must be absolute and may not contain empty, `.` or `..` components.

Backquoted commands are also accepted in an explicit recipe destination:

```text
:0
maildir:`date +%Y-%m`/
```

As with assignment backquotes, each command receives the complete current
message, all trailing LF bytes are removed from its stdout, and the complete
result is kept private until every literal expansion and command succeeds.
The raw captured output is limited by the smaller of active `LINEBUF` and the
4096-byte path-expression ceiling. The result must be UTF-8 and pass the normal
path checks; relative results use the `MAILDIR` active when the recipe executes.
Bytes emitted by a command are inserted literally and are not scanned again as
variable references. `TIMEOUT`, the bounded child environment, and `LOGFILE`
stderr handling are identical to assignment backquotes.

## Explicit stdout delivery

A recipe whose complete action is `|` delivers its selected input directly to
procmail-rs stdout without starting a shell. The `h` and `b` flags select the
header or body; neither selects the complete message. Without `r`, a missing
final LF is added, while `r` preserves the exact ending. A stdout write or flush
failure makes the recipe fail unless `i` is present. The ordinary `c`, `A`,
`a`, `E`, and `e` flow rules continue to apply. The filter form `:0 f` followed
by a sole `|` is rejected because stdout does not provide a replacement message.

This action is evaluated only after the selected message bytes have passed all
input limits. It does not enable `DEFAULT=|` or any other implicit delivery.

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
rc expansion forms `$NAME`, `${NAME}`, `${NAME-word}`, `${NAME:-word}`,
`${NAME+word}`, and `${NAME:+word}` when the action executes. NUL, CR, LF, and
requested folded continuations are rejected.
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

Reserved procmail variables `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`,
`DROPPRIVS`, `LOG`, `MSGPREFIX`, `NORESRETRY`,
`PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`, `SENDMAIL`, `SENDMAILFLAGS`, and
`SHIFT`, as well as the project-reserved `LIMIT_RC_SIZE`, are rejected by name.
Forward actions beginning with `!` are also rejected. This makes unsupported
behavior visible instead of silently assigning it another meaning.

## Deliberate differences

| Area | procmail 3.22 | procmail-rs |
| --- | --- | --- |
| Native header editing | Requires an external filter such as `formail`; there is no `headers { ... }` action. | Provides the bounded `headers` extension described above. Rc files using it are intentionally not accepted by procmail 3.22. |
| Destination type and directory delivery | May infer a directory or mailbox from the current filesystem. | Never infers a backend from the filesystem. Requires `maildir:PATH` or a trailing `/` for a Maildir containing `tmp`, `new`, and `cur`; `mbox:PATH` and every other unmarked path select mbox. An unmarked path resolving exactly to `/dev/null` is discarded internally after complete input validation. |
| Default delivery | Can fall back to `DEFAULT`, `ORGMAIL`, or the system mailbox. | Never selects an implicit destination. An undelivered original is an error. |
| Forwarding | A `!` action forwards through the configured sendmail command. | Rejected before message input; procmail-rs never forwards or invokes sendmail implicitly. |
| Comsat notification | `COMSAT` may enable notification after delivery. | `COMSAT` is rejected as an unsupported reserved variable; delivery has no notification side effect. |
| `HOST` | Initializes the variable from the current hostname, continues on an exact match, and ends the current rc file on a mismatch. | Implements the same control flow using the bounded UTF-8 node name returned by `uname`. Ambient environment values cannot replace it. Node names that are empty, invalid UTF-8, or longer than 255 bytes are rejected before message input. |
| Runtime rc files | Opens paths using the process filesystem permissions. | Requires trusted regular files owned by the current uid and rejects broadly writable files and symlinks. |
| Initial variables | Imports a broad process environment. | Gets `HOME` and `LOGNAME` from the current uid and accepts other external values only through `--set`. |
| `PROCMAIL_VERSION` | Contains the running procmail version number and cannot be changed. | Contains the bounded package version from `Cargo.toml` and cannot be changed. The value identifies procmail-rs and does not claim to be procmail 3.22. |
| Unsupported reserved variables | Variables such as `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`, `DROPPRIVS`, `LOG`, `MSGPREFIX`, `NORESRETRY`, `PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`, `SENDMAIL`, `SENDMAILFLAGS`, and `SHIFT` retain their original special meanings. | Rejects these names and the project-reserved `LIMIT_RC_SIZE` explicitly in assignments, `--set`, and expansion references. Unknown names remain ordinary user variables. |
| `LOGABSTRACT` | Defaults to a final abstract containing `From`, `Subject`, destination, and message size; `no` suppresses it and `all` logs every successful delivery. | Accepts only the exact value `no`, including after bounded variable expansion. Abstract logging remains disabled because other modes could expose sensitive header values. A statically known unsupported value is rejected before message input; a runtime-derived value is rejected when its selected assignment executes. |
| Pipe command parsing | Uses a hybrid direct-command and shell parser. | Runs every trusted pipe command through the configured, policy-checked shell. |
| Captured NUL bytes | A NUL from a backquoted command terminates the assigned value. | Preserves NUL as variable data. A later external command cannot receive such a value because operating-system environment entries cannot contain NUL. |
| Command-output bounds | `LINEBUF` overflow may truncate data and set `PROCMAIL_OVERFLOW`; waits may be unbounded under compatible settings. | Rejects raw stdout beyond the active `LINEBUF` or destination-variable ceiling and always applies finite `TIMEOUT` supervision. No partial value is assigned. |
| mbox in general-filter mode | A bare output file does not gain a generated postmark. | Explicit `mbox:` delivery always writes a complete mboxrd record with a generated postmark. |
| `i` on mbox or Maildir | May ignore a failed write and report success after a partial append or publish a truncated Maildir file. | Rejected before message input. Filesystem publication must complete successfully. |
| `i` on a recipe block | Ignored with a warning. | Ignored with a source-located warning; filesystem delivery still rejects `i`. |
| `r` on mbox | Raw file delivery suppresses the usual mailbox delimiter handling as well as final-newline normalization. | Retains the generated postmark and mboxrd quoting. It omits the normal blank record separator but adds one LF when needed so a following postmark starts on a new line. |
| `r` on a recipe block | Ignored with a warning. | Ignored with a source-located warning. |
| Local recipe lockfiles | Creates and later removes a named dotlock, or derives its name from the destination. | Defaults to a persistent, ownership-checked file held with `flock`. `LOCKMETHOD=dotlock` selects compatible creation, stale removal, and cleanup with the original pathname-replacement risk. |
| `LOCKEXT` | Defaults to `.lock` and is appended when deriving a local lockfile name. | Preserves the default and statement-order assignment. The suffix may be empty, is bounded to 4096 bytes, may not contain NUL or `/`, and the complete derived path remains bounded to 4096 bytes. |
| Implicit pipe lockfile | Attempts to derive a name from redirection found in the command. | Rejected before message input; shell command text is not reinterpreted to guess a lock path. |
| Lockfile on a recipe block | Documents that a lock on a non-forking block does not work as expected; procmail 3.22 was observed creating and removing the dotlock before the child sequence and logging `Extraneous locallockfile ignored`. | Requires an explicit lockfile name and holds the selected `flock` or dotlock across the complete child sequence. The path and active `LOCKMETHOD`, `LOCKSLEEP`, `LOCKTIMEOUT`, `UMASK`, variables, and `MAILDIR` are resolved when the block is selected. An implicit block lock is rejected because no single destination exists from which to derive it. |
| `LOCKFILE` | Replaces the preceding global dotlock and holds the new one until replacement or exit. | Preserves statement-order lifetime while using the active `LOCKMETHOD`; flock remains the default. |
| `LOCKSLEEP=0` | Retries without a delay. | Rejected to prevent active retry loops. The compatible 8-second default and values from 1 through 86400 seconds control local, global, and mbox lock retry intervals. |
| `LOCKTIMEOUT=0` | Waits indefinitely without stale-dotlock removal. | Rejected because all lock waits must remain finite. Values from 1 through 86400 seconds are accepted and also bound mbox flock waits. |
| `LINEBUF` | Defaults to 2048, has a minimum of 128, and may be changed while an rc file executes. Overflow may truncate data and set `PROCMAIL_OVERFLOW`. | Rejects overflow instead of truncating it, has a 1048576-byte ceiling, and accepts only literal top-level assignments because the complete typed recipe tree is built before message filtering. Mail input and trace limits remain separate. |
| `TIMEOUT=0` | Waits indefinitely for child termination. | Rejected because process waits must remain finite. The 960-second default and values from 1 through 86400 are supported. |
| `UMASK` | Changes the process umask and may permit group or other access when configured accordingly. | Accepts octal `0000` through `0777` in statement order, but only removes bits from restrictive backend modes. The process-wide umask is not changed and may remove more bits. |
| `TRAP` input | Runs on normal termination with the current message and appends one LF. | Matches this behavior after complete-input validation and supplies the final filtered message through bounded staging or the filter-owned buffer. It does not run after rejected partial input. |
| `TRAP` output and status | Sends stdout to the logging descriptor and can replace the result when `EXITCODE` is empty. | Sends both stdout and stderr to `LOGFILE`, applies bounded `TIMEOUT`, and preserves the recorded unset, empty, and explicit `EXITCODE` behavior. Start failure or timeout becomes status 75 only when `EXITCODE` is empty. |
| Termination signals | Handles termination while reading or processing a message according to its internal signal state. | `SIGHUP`, `SIGINT`, `SIGQUIT`, and `SIGTERM` interrupt message input and lock waits, terminate an active external process group, suppress `TRAP` and error-recipe recovery, and return `128 + signal`. Private staging is removed when it is still owned. A signal observed before publication prevents publication; a signal arriving after an atomic Maildir publication or completed mbox append cannot retract that delivery. |
| Timed-out descendants | Sends `SIGTERM` to the child selected by procmail's process tracking. | Runs each shell in a separate process group, then sends `SIGTERM` and `SIGKILL` to that group. A trusted command that deliberately leaves the group still requires external cgroup or namespace containment. |

## Updating this document

Add compatible behavior to the implemented table and add intentionally narrowed
or safer behavior to the differences table. Each entry needs focused tests that
exercise the procmail-rs behavior; where practical, store a reviewed reference
result without making the original executable a test dependency.
