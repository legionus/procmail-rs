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
| Variable references | `$NAME`, `${NAME}`, positional `$N` and `${N}`, argument count `$#` and `${#}`, `${NAME-word}`, `${NAME:-word}`, `${NAME+word}`, `${NAME:+word}`, `${NAME:=word}`, `${NAME:?word}`, `${#NAME}`, pattern removal with `#`, `##`, `%`, and `%%`, and ASCII case forms with `^`, `^^`, `,`, and `,,`; see [ShellExpressions.md](ShellExpressions.md) |
| Recipe header | `:0` followed by flags and an optional `: lockfile` |
| Condition source flags | default/`H` for normalized headers, `B` for body, and `HB` for their documented combined byte sequence |
| Recipe flags | `H`, `B`, `D`, `c`, `A`, `a`, `E`, `e`, `h`, `b`, `f`, `w`, `W`, `i`, and `r`, subject to action-specific checks |
| Conditions | Byte regex, leading `!` negation, shell-expanded `$` conditions, `? shell command`, `< size`, `> size`, `H ?? regex`, `B ?? regex`, `$NAME ?? regex`, and the procmail-rs structured `address` and `identifier` extensions |
| Actions | Explicit Maildir or mbox delivery, including bounded backquoted destination commands; explicit discard through an unmarked `/dev/null`; trusted shell pipe action; a sole `|` for stdout delivery; command-output capture with `NAME=| command`; `{ ... }` block; and the procmail-rs `headers { ... }` extension |
| Regex dialect | One byte-oriented Rust `regex-syntax` dialect with counted repetition, named ASCII classes, and the procmail `^TO`, `^TO_`, `^FROM_DAEMON`, `^FROM_MAILER`, `\<`, `\>`, `^^`, and `\/` extensions. Capturing groups populate numbered `MATCH1` values through the configured capture ceiling. |
| Runtime files | Conditional and nested `INCLUDERC`; `SWITCHRC` abandons the current rc file after a successful switch |
| Root rc discovery | An omitted `--config` searches the passwd-derived `HOME` for `.config/procmail-rs/config` and then `.procmailrc`. | Ambient `HOME` and `XDG_CONFIG_HOME` values are ignored. A preferred file which exists but cannot be loaded is an error rather than a reason to fall back. |
| External values | Passwd-derived `HOME` and `LOGNAME`, system-derived `HOST`, read-only `PROCMAIL_VERSION`, policy-checked `--set` values, and repeatable `-a` positional arguments; ambient process variables are not imported |
| Logging | `LOGFILE`, `VERBOSE`, `LOGABSTRACT`, and `LOGDETAIL=values`; text and JSON formats include session boundaries, recipe decisions, native header operations, and delivery outcomes |
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
| Structured address and list identifier matching | Original procmail has no corresponding condition and recipes normally match encoded header text or invoke an external parser. | procmail-rs adds the bounded `address` and `identifier` forms documented in [StructuredConditions.md](StructuredConditions.md). |
| A condition beginning with `$` is expanded using shell substitution rules inside double quotes and then reparsed as a condition. | Supported with the project's bounded variable syntax, procmail's `$\NAME` regex quoting, and backquoted commands. | Intermediate text is limited by active `LINEBUF`; commands receive the complete current message and use active `TIMEOUT` and `LOGFILE`. Unsupported special parameters are rejected explicitly. Runtime-dependent forms conservatively require complete staging because the resulting condition type is not known before evaluation. |
| `w^x` weighted regex, program, and length conditions; final score in `$=` | Explicitly rejected as one unsupported condition category. | Implement all scoring forms together with bounded match counting, checked numeric handling, and `$=` so a mixed recipe cannot receive partial scoring behavior. |
| A trailing backslash continues a condition; shell-expanded conditions retain continuation whitespace. | Supported. Each physical line and the complete joined condition are bounded by the active `LINEBUF`. | Leading space and tab are removed from continued ordinary conditions and retained in `$` conditions before their expansion pass. |
| Procmail ERE operators and its `^`, `$`, `^^`, `\<`, `\>`, and `\/` extensions | Partly supported by parsing the source with upstream `regex-syntax`, locating extensions through typed AST nodes and source spans, applying bounded replacements, and compiling with the Rust byte-regex engine. | procmail-rs deliberately uses the richer Rust regex dialect. Counted repetition uses `{m,n}` and literal braces must be escaped, while original procmail treats braces as ordinary text. Named ASCII classes and other documented Rust forms are accepted. Original procmail selects the leftmost shortest span before `\/` and the leftmost longest suffix after it; the Rust engine is leftmost-first. This difference is observable when alternatives are ambiguous, as recorded in `tests/fixtures/regex_match_selection/reference-behavior.md`. |
| `\/` inside groups, alternatives, and expressions containing several markers | Supported with at most 64 markers per expression. Focused tests preserve results recorded with Debian-patched procmail 3.23pre. | Nested markers include the complete matched suffix and a terminal LF consumed by `$`. A successful alternative which does not reach a marker retains the old `MATCH`; if a successful path reaches several markers, the last reached marker selects its value. |
| Shell-style assignments, including quoting, escapes, unsetting, field splitting, and parameter forms | procmail-rs supports the bounded expression language documented in [ShellExpressions.md](ShellExpressions.md), including several forms beyond original procmail. | Bare-name unsetting and field splitting of command output remain absent. Unsupported forms, extra words, and unterminated quotes are rejected instead of being assigned a different meaning. |
| Repeatable `-a argument`, numbered `$1`, `$2`, ... parameters, `$#`, and `SHIFT` | Supported through 256 arguments, with per-value and aggregate byte ceilings. Missing numbered parameters expand to empty. A positive decimal `SHIFT` advances the remaining argument window and caps an excessive shift at `$#`. | `$@` and `$*` remain explicitly unsupported. Positional values and shifts cross runtime include and switch boundaries but are not exported as numeric child-environment names. Copy branches shift their private runtime state. Unlike original procmail, zero, negative, and malformed `SHIFT` values are rejected instead of being ignored. |
| Backquoted commands in assignments and in mailbox names | Supported. Each command receives the complete current message and its stdout participates in bounded path construction. | Destination output must be UTF-8 and obeys both active `LINEBUF` and the fixed path ceiling. It is resolved only after every fragment succeeds. |
| `| command`, `NAME=| command`, and a sole `|` that writes the selected input to stdout | All three explicit recipe forms are supported. | A sole `|` writes the area selected by `h`/`b` directly, applies `r` and `i`, and does not start a shell. `DEFAULT=|` remains outside project scope because implicit fallback delivery is absent. |
| A pipe without `w` or `W` may continue without waiting after its input has been accepted. | Supported for non-filter pipe actions. | The complete selected input is written before recipe evaluation continues. Process-group timeout supervision, stderr redirection, reaping, and local-lock ownership remain active in the background. All background commands are reaped before message processing returns, with at most 128 background commands per message. Filters and captures still wait because their output is needed immediately. |
| `c` on a nesting block clones processing and lets the parent skip the block. | Supported with branch-local variables, current-message changes, global locks, and runtime rc paths. | Plain `c` runs the complete branch concurrently with the parent and joins it before message processing returns. `cw`, `cW`, and a block with a local lock wait before the parent continues, as original procmail does. At most 128 background copy branches may be started per message. Branch action failures do not change plain `c` parent status; configuration, resource-limit, and supervision failures still fail the message. |
| `h` or `b` on file delivery writes only the selected part and may discard the other part. | Explicitly rejected for filesystem delivery. | This is a deliberate data-loss prevention measure. Keep it as an explicit difference unless partial-message delivery becomes an opt-in feature. |
| Mailbox actions may contain several directory destinations, ordinary directory folders, MH folders ending in `/.`, or Maildir folders ending in `/`. | Only one mbox or Maildir target is accepted; ordinary directory folders, MH folders, and multi-folder hardlink delivery are absent. | Whitespace in an unmarked destination is rejected both before and after variable expansion. Explicit `mbox:` and `maildir:` paths may contain whitespace because their backend and single-target meaning are unambiguous. |
| Existing directories can select directory delivery even without a suffix. | Deliberately not inferred from filesystem state. | Use `maildir:PATH` or a trailing `/`; every other bare path deterministically selects mbox. |
| `INCLUDERC` and `SWITCHRC` execute in statement order; empty `SWITCHRC` ends the current rc file; `/dev/null` is a valid switch target. | Ordered include, switch, empty switch, and a resolved `SWITCHRC=/dev/null` are supported with bounded runtime loading. | The null switch counts toward the transition limit and ends the current rc file without opening the device. `INCLUDERC=/dev/null` remains subject to the regular-file policy. |
| Old `:n` recipe headers and unlimited nesting | Only `:0` and bounded nesting are accepted. | Both differences fail explicitly and do not risk a different delivery. |

### Documented variables

| Status | Variables | Notes |
| --- | --- | --- |
| Supported or intentionally narrowed | `HOME`, `LOGNAME`, `PATH`, `SHELL`, `SHELLFLAGS`, `MAILDIR`, `LOGFILE`, `VERBOSE`, `LOGABSTRACT`, `LOCKFILE`, `LOCKEXT`, `LOCKSLEEP`, `LOCKTIMEOUT`, `TIMEOUT`, `HOST`, `UMASK`, `TRAP`, `EXITCODE`, `LASTFOLDER`, `MATCH`, `INCLUDERC`, `SWITCHRC`, `PROCMAIL_VERSION`, and `LINEBUF` | Exact restrictions are recorded in this document and in the limits documentation. `MATCH1`, `MATCH2`, and later numbered captures are procmail-rs additions. |
| Explicitly rejected | `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`, `DROPPRIVS`, `LOG`, `MSGPREFIX`, `NORESRETRY`, `PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`, `SENDMAIL`, `SENDMAILFLAGS`, and `LIMIT_RC_SIZE` | These names cannot accidentally act as ordinary variables. Implementing `DROPPRIVS` is outside project scope. `LIMIT_RC_SIZE` cannot safely configure the read which has already consumed its own assignment. |
| Original startup environment behavior | `IFS`, `ENV`, and `PWD` are cleared or preset, and other ambient variables are generally imported. | procmail-rs instead builds a bounded child environment from its runtime variable table. This is deliberate, but rc assignments with these names remain ordinary exported variables. |

### Coverage of `procmailex(5)` patterns

The common examples using regex selection, mbox delivery, Maildir delivery,
copy recipes, `A`/`a`/`E`/`e`, program conditions, external filters,
command-output assignments, destination command substitution, pipe-to-stdout,
shell-expanded conditions, `MATCH`, `TRAP`, and `EXITCODE` have corresponding
implementation paths. The manual's forwarding and autoreply examples remain
outside project scope. Its scoring, directory-folder, MH, and multi-folder
examples expose the gaps listed above.

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
general shell evaluation. A child shell uses the active non-empty `MAILDIR` as
its working directory, matching the paths visible to commands under original
procmail. Procmail-rs applies that directory to each child instead of changing
the parent process directory, so concurrent copy branches cannot affect one
another.

When `VERBOSE` is enabled, text traces begin with a PID and timestamp and
report successful destinations as `Delivered to ...`. Destination paths and
command text are shown only with `LOGDETAIL=values`; JSON uses one object per
line and carries the same detail policy. Native header operations identify
field names and source lines, but never expose header values. Values extracted
from headers remain hidden even in detailed mode.

## Extension documentation

Detailed descriptions of procmail-rs-only syntax live outside this comparison:

- [Extensions.md](Extensions.md) provides the overview;
- [ShellExpressions.md](ShellExpressions.md) defines bounded rc expansion;
- [HeaderEditing.md](HeaderEditing.md) describes the `headers { ... }` action;
- [StructuredConditions.md](StructuredConditions.md) describes `address` and `identifier`.

The complete rc manual remains self-contained and includes both compatible and
extended syntax.

## Deliberate differences

| Area | procmail 3.22 | procmail-rs |
| --- | --- | --- |
| Native header editing | Requires an external filter such as `formail`; there is no `headers { ... }` action. | Provides the bounded extension documented in [HeaderEditing.md](HeaderEditing.md). Rc files using it are intentionally not accepted by procmail 3.22. |
| Destination type and directory delivery | May infer a directory or mailbox from the current filesystem. | Never infers a backend from the filesystem. Requires `maildir:PATH` or a trailing `/` for a Maildir containing `tmp`, `new`, and `cur`; `mbox:PATH` and every other unmarked path select mbox. An unmarked path resolving exactly to `/dev/null` is discarded internally after complete input validation. |
| FreeBSD Maildir publication | Creates and renames a named file in `tmp`. | Uses exclusive named creation in `tmp`, no-replace hard-link publication in `new`, and inode checks before cleanup. It additionally requires the Maildir directories to be owned by the current uid and not writable by group or other users. Without FreeBSD-specific FFI there remains a pathname-replacement race between each check and operation; Linux uses the stronger unnamed-file path. |
| Default delivery | Can fall back to `DEFAULT`, `ORGMAIL`, or the system mailbox. | Never selects an implicit destination. An undelivered original is an error. |
| Forwarding | A `!` action forwards through the configured sendmail command. | Rejected before message input; procmail-rs never forwards or invokes sendmail implicitly. |
| Comsat notification | `COMSAT` may enable notification after delivery. | `COMSAT` is rejected as an unsupported reserved variable; delivery has no notification side effect. |
| `HOST` | Initializes the variable from the current hostname, continues on an exact match, and ends the current rc file on a mismatch. | Implements the same control flow using the bounded UTF-8 node name returned by `uname`. Ambient environment values cannot replace it. Node names that are empty, invalid UTF-8, or longer than 255 bytes are rejected before message input. |
| Runtime rc files | Opens paths using the process filesystem permissions. | Requires trusted regular files owned by the current uid and rejects broadly writable files and symlinks. |
| Initial variables | Imports a broad process environment. | Gets `HOME` and `LOGNAME` from the current uid and accepts other external values only through `--set`. |
| `PROCMAIL_VERSION` | Contains the running procmail version number and cannot be changed. | Contains the bounded package version from `Cargo.toml` and cannot be changed. The value identifies procmail-rs and does not claim to be procmail 3.22. |
| Unsupported reserved variables | Variables such as `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`, `DROPPRIVS`, `LOG`, `MSGPREFIX`, `NORESRETRY`, `PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`, `SENDMAIL`, and `SENDMAILFLAGS` retain their original special meanings. | Rejects these names and the project-reserved `LIMIT_RC_SIZE` explicitly in assignments, `--set`, and expansion references. Unknown names remain ordinary user variables. |
| `LOGABSTRACT` | Defaults to a final abstract containing `From`, `Subject`, destination, and message size; `no` suppresses it and `all` logs every successful delivery. | Accepts only the exact value `no`, including after bounded variable expansion. Abstract logging remains disabled because other modes could expose sensitive header values. A statically known unsupported value is rejected before message input; a runtime-derived value is rejected when its selected assignment executes. |
| Pipe command parsing | Uses a hybrid direct-command and shell parser. | Runs every trusted pipe command through the configured, policy-checked shell. |
| Captured NUL bytes | A NUL from a backquoted command terminates the assigned value. | Preserves NUL as variable data. A later external command cannot receive such a value because operating-system environment entries cannot contain NUL. |
| Command-output bounds | `LINEBUF` overflow may truncate data and set `PROCMAIL_OVERFLOW`; waits may be unbounded under compatible settings. | Rejects raw stdout beyond the active `LINEBUF` or destination-variable ceiling and always applies finite `TIMEOUT` supervision. No partial value is assigned. |
| mbox in general-filter mode | A bare output file does not gain a generated postmark. | Explicit `mbox:` delivery always writes a complete mboxrd record with a generated postmark. |
| `i` on mbox or Maildir | May ignore a failed write and report success after a partial append or publish a truncated Maildir file. | Rejected before message input. Filesystem publication must complete successfully. |
| `i` on a recipe block | Ignored with a warning. | Ignored with a source-located warning; filesystem delivery still rejects `i`. |
| `r` on mbox | Raw file delivery suppresses the usual mailbox delimiter handling as well as final-newline normalization. | Retains the generated postmark and mboxrd quoting. It omits the normal blank record separator but adds one LF when needed so a following postmark starts on a new line. |
| `r` on a recipe block | Ignored with a warning. | Ignored with a source-located warning. |
| Local recipe lockfiles | Creates and later removes a named dotlock, or derives its name from the destination. | Defaults to a persistent, ownership-checked file held with `flock`. `LOCKMETHOD=dotlock` selects compatible creation, stale removal, and cleanup with the original pathname-replacement risk. On an mbox recipe, this dotlock is held outside the mandatory mbox `flock`, providing a transition mode for writers which cooperate through either mechanism. |
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
