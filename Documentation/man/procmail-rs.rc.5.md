<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# NAME

procmail-rs.rc - configuration language for procmail-rs

# DESCRIPTION

An rc file is a UTF-8 sequence of assignments and recipes evaluated in source
order. It selects how **procmail-rs**(1) examines and delivers one byte-oriented
mail message. The accepted language is intentionally smaller than procmail
3.22; unsupported recognized forms are rejected instead of being approximated.

The root file is completely parsed and validated before message input is read.
An `INCLUDERC` or `SWITCHRC` whose path depends on message processing is loaded
when execution reaches it and is subjected to the same aggregate limits.

# LEXICAL RULES

Blank lines and lines whose first non-whitespace byte is `#` are ignored.
Assignments have the form `NAME=value`. A recipe begins with `:0`, contains
zero or more condition lines beginning with `*`, and ends in one action.

A standalone comment may occur between recipe conditions. An assignment value
is one shell-like word. After that word, optional whitespace followed by `#`
begins a comment. A `#` within the word, inside either quote mode, after a
backslash, or inside a backquoted command is data. A second unquoted word is
rejected instead of being ignored. A recipe header may also have a trailing
comment. Do not append an rc comment to a condition or destination action:
there it belongs to the regex, path, or shell text being parsed.

Pipe commands and recipe conditions may span physical lines. Every continued
pipe-command line retains its trailing backslash for the real shell. A
condition's trailing backslash and physical newline are removed; leading space
and tab on the following line are removed for ordinary conditions and retained
for shell-expanded `$` conditions. Each physical line and the complete joined
command or condition are bounded by `LINEBUF`. Unquoted, single-quoted, and
double-quoted assignment fragments
may be concatenated without whitespace, as in `NAME=pre-'literal-'"$VALUE"`.

Physical rc lines, expanded values, and continued conditions and pipe commands
are bounded by the active `LINEBUF`. The root rc file itself is bounded
independently.

# VARIABLE EXPANSION

The supported forms are `$NAME`, `${NAME}`, and
`${NAME:-expression}`. A name begins with an ASCII letter or underscore and
continues with ASCII letters, digits, or underscores. The `:-` expression is
used only when the named value is absent or empty.

Inserted variable bytes are literal and are not scanned for another expansion.
Outside quotes, backslash makes the next byte literal. Inside double quotes,
it quotes only `$`, backquote, double quote, backslash, and newline; before
another byte the backslash is retained. Inside single quotes, every byte is
literal until the next single quote: variables are not expanded, backquoted
commands are not run, and backslash has no special meaning.

```
MAILDIR=/srv/mail
FOLDER=${ACCOUNT:-personal}
LABEL='literal $ACCOUNT'-"-$FOLDER"

:0
maildir:$MAILDIR/$FOLDER/
```

Field splitting, globbing, tilde expansion, arithmetic substitution, special
parameters, and other parameter operators are not rc expansion features.
Shell command text is different: it is interpreted by the trusted shell
selected by `SHELL` and `SHELLFLAGS`.

# ASSIGNMENTS

An ordinary assignment replaces the runtime value used by following
statements. Top-level settings that affect parsing, such as `LINEBUF` and the
`LIMIT_RC_*` family, apply only to following source text. Parser-limit
assignments are rejected inside recipe blocks.

Backquotes run a trusted shell command when the assignment is reached. The
command receives the complete current message. All trailing LF bytes are
removed from its bounded stdout before fragments are joined.

```
DOMAIN=`printf '%s\n' "$MATCH1" | tr A-Z a-z`
ARCHIVE=${DOMAIN:-unknown}
```

No partial value becomes visible if expansion, execution, or output validation
fails.

# RECIPES

The general form is:

```
:0 [FLAGS] [: [LOCKFILE]]
* CONDITION
* CONDITION
ACTION
```

All conditions must match. With no conditions, the recipe matches immediately.
A delivering recipe stops processing the original message; a `c` recipe acts
on a copy and permits later recipes to continue.

```
:0
* ^Subject:.*release
releases/

:0
inbox/
```

## Recipe flags

Condition flags and action flags are independent: uppercase `H` and `B` select
where conditions search, while lowercase `h` and `b` select bytes passed to an
action. Repeating a flag has no additional effect.

`H`

: Search normalized headers. This is the condition default when neither `H`
  nor `B` is present. Header folding is normalized only for matching; delivery
  retains the original bytes.

`B`

: Search the body. `HB` or `BH` searches one combined area containing
  normalized headers, their separator, and the body.

`D`

: Distinguish ASCII uppercase and lowercase in regex conditions. Matching is
  case-insensitive by default. `^FROM_DAEMON` remains case-insensitive because
  that procmail macro requires it.

`A`

: Run only if the last preceding recipe at the same nesting level which did
  not itself have `A` or `a` had matching conditions. This permits several
  actions to share one test.

`a`

: Like `A`, and additionally require the immediately preceding recipe action
  to have succeeded.

`E`

: Run only if the immediately preceding recipe was not executed. A selected
  `E` recipe prevents a following `E` recipe, forming an else-if chain.

`e`

: Run only if the immediately preceding recipe action was attempted and
  failed. A recipe skipped because its conditions did not match is not an
  error.

`c`

: Deliver or process a copy, then continue with later recipes. A copy-only
  result does not count as delivery of the original. `c` on a recipe block is
  rejected because process-cloning block behavior is not implemented.

`h`

: Pass only the header section to a pipe, filter, capture, or sole-`|` stdout
  action.

`b`

: Pass only the body to one of those actions. With neither or both of `h` and
  `b`, pass the complete message. Partial-area filesystem delivery is rejected.

`f`

: Treat a pipe as a filter. Successful bounded stdout replaces the action area;
  unselected message bytes are retained. This flag requires a non-empty pipe
  command.

`w`

: Wait for a child and make an ordinary nonzero status fail the action. The
  failure is diagnosed and may select a following `e` recipe.

`W`

: Apply the same status handling as `w`, but suppress the child-failure
  diagnostic. `w` and `W` cannot be combined.

`i`

: Ignore only an error encountered while writing selected input to a child or
  direct stdout. It does not ignore child status, timeout, invalid filter
  output, or output-limit failure. It is rejected on filesystem delivery so a
  partial message cannot be reported as delivered.

`r`

: Preserve the selected input ending for pipe-like actions. Without `r`, the
  action input receives procmail-compatible terminating LF bytes. Maildir
  already preserves the message, while mbox retains its required postmark and
  framing even with `r`.

Flags that have no meaning for an action are either rejected or, for the
documented block cases, ignored with a source-located warning. In particular,
`i`, `h`, and `b` cannot weaken filesystem delivery.

This chain delivers command failures to a fallback Maildir:

```
:0 fw
| bogofilter -p -u -e

:0 e
failed-filter/

:0
* ^X-Bogosity: (Spam|Yes)
spam/
```

Uppercase chaining can reuse one condition without repeating its regex. The
lowercase `a` additionally depends on successful completion of the immediately
preceding action:

```
:0 c
* ^X-Project: compiler
projects/compiler/

:0 Ac
mbox:project-audit

:0 a
projects/notified/
```

An `E` recipe supplies an else branch when the preceding recipe was skipped:

```
:0
* ^List-Id:
lists/

:0 E
ordinary/
```

# CONDITIONS

## Regular expressions

A plain condition searches the area selected by the recipe. Matching operates
on bytes and ignores ASCII case unless `D` is present. It is not Unicode
matching: character classes, `.` and case folding operate on message bytes.
Prefix `!` negates any supported condition and repeated `!` prefixes alternate
the result.

```
:0 D
* ! ^X-Verified: yes$
unverified/
```

`H ?? REGEX`, `B ?? REGEX`, and `HB ?? REGEX` or `BH ?? REGEX` override the
recipe search area for one condition. `$NAME ?? REGEX` searches the current
byte value of a variable.

```
:0 B
* H ?? ^Content-Type: text/plain
* urgent
urgent-body/
```

The ordinary operators are:

`literal`

: Match the same literal byte sequence. A backslash quotes a regex metabyte.

`.`

: Match one byte other than LF.

`[abc]`, `[^abc]`, `[a-z]`, `[[:digit:]]`

: Match one listed, excluded, ranged, or named ASCII byte class.

`a?`, `a*`, `a+`

: Match zero or one, zero or more, or one or more repetitions.

`a{m}`, `a{m,}`, `a{m,n}`

: Counted repetition in the Rust regex dialect. Original procmail treats braces
  as ordinary text; escape them as `\{` and `\}` when literal braces are
  intended.

`ab|cd`, `(expression)`

: Alternation and grouping. Capturing groups populate numbered `MATCHn`
  variables after a successful condition. Noncapturing groups `(?:...)` are
  accepted. Named groups, look-around, and backreferences are rejected.

The dialect also accepts ASCII `\d`, `\s`, `\w` classes and their
uppercase negations, zero-width `\b` and `\B`, hexadecimal byte escapes, lazy
repetition, and scoped flag groups such as `(?i:expression)`. Some of these
forms have no equivalent in original procmail. Unicode properties are
unavailable because matching operates on bytes.

The following anchors and macros have procmail-specific handling:

`^`, `$`

: Match the start or end of a line or the edge of the selected search area.
  At an internal line boundary they consume the separating LF, matching the
  documented procmail multiline behavior.

`^^`

: At the start of an expression, match only the start of the complete selected
  area. At the end, match only its end. `^^` elsewhere is rejected.

`\<`, `\>`

: Match and consume one byte outside `[A-Za-z0-9_]`, including LF. These are
  procmail consuming word-edge forms, not zero-width word boundaries.

`\/`

: Mark the start of the suffix assigned to `MATCH`. Up to 64 `\/` markers may
  occur in one expression, including inside groups and alternatives. The last marker
  reached by the successful path selects the value; a successful alternative
  which does not reach any marker preserves the preceding `MATCH`. The value
  extends to the end of the complete regex match, so it includes an LF consumed
  by a terminal `$`.

`^TO`, `^TO_`

: Expand to procmail-compatible recipient-header expressions. `^TO` uses a
  word-style address boundary; `^TO_` uses the broader address boundary. The
  macro is recognized wherever it occurs unless its caret is immediately
  preceded by backslash.

`^FROM_DAEMON`, `^FROM_MAILER`

: Expand to the fixed procmail daemon or mailer-sender expressions.
  `^FROM_DAEMON` forces case-insensitive matching.

Text matched after `\/` is assigned to `MATCH`; ordinary capture groups are
assigned to `MATCH1`, `MATCH2`, and so on, excluding the private groups used to
locate `MATCH` markers. An unmatched optional group becomes an empty value. At most
64 ordinary capture groups are accepted and one captured value is bounded to
64 KiB.

```
:0 c
* ^List-Id:.*<\/([^.>]+)\.([^.>]+)>
maildir:lists/$MATCH2/$MATCH1/
```

The complete user pattern is bounded to 64 KiB, the parsed regex tree to 256
levels, and its compiled form to 8 MiB. Generated text for the four fixed
macros does not consume `LINEBUF`, but does consume the translated-pattern and
compiled-size ceilings.

Procmail chooses the leftmost shortest match except while selecting `MATCH`.
For a condition containing `\/`, it selects the leftmost shortest span before
the reached marker and the leftmost longest suffix after it. The
**procmail-rs** engine is leftmost-first, so ambiguous alternatives and
repetitions can select different spans, marker paths, and numbered captures.
Alternative order therefore matters in **procmail-rs** even where it did not
affect original procmail. Weighted scoring conditions and their `$=` result are
rejected. See the compatibility document before migrating expressions that
depend on ambiguous match selection.

```
:0
* ^Subject: [[:digit:]]{3}$
numbered-subject/
```

## Size conditions

`< NUMBER` matches when the complete message is smaller than the decimal byte
count. `> NUMBER` matches when it is larger.

```
:0
* > 10485760
large/
```

## Program conditions

`? COMMAND` sends the complete current message to a trusted shell command and
matches status zero. `TIMEOUT`, `LOGFILE`, and the rebuilt bounded child
environment apply.

```
:0
* ? test -e "$HOME/.accept-mail"
accepted/
```

## Shell-expanded conditions

A condition beginning with `$` expands its remainder using the supported
double-quoted rules and reparses the bounded result as a condition. `$\NAME`
quotes the selected variable as literal regex bytes, preventing its contents
from becoming condition syntax.

```
ADDRESS=user@example.org

:0
* $^To:.*<$\ADDRESS>
addressed/
```

A backquoted command in this form receives the complete current message. Every
intermediate expansion remains subject to `LINEBUF` and the destination
condition's own limits.

# ACTIONS

## Maildir and mbox

`maildir:PATH` and a path ending in `/` select Maildir. `mbox:PATH` and every
other unmarked path select mboxrd. The unmarked path `/dev/null` is the sole
discard action. Backend selection never depends on the current filesystem
type.

Relative paths use `MAILDIR` active when the statement executes, or the process
working directory when it is unset.

```
MAILDIR=/srv/mail

:0 c
maildir:audit/

:0
mbox:archive/received.mbox
```

Maildir directories must already contain `tmp`, `new`, and `cur`. Filesystem
paths reject NUL, empty components, `.`, `..`, repeated separators, and
unexpected trailing separators.

## Pipes and filters

An action beginning with `|` runs its remainder through the configured trusted
shell. The selected message bytes are written to stdin. A successful `f`
recipe replaces the selected message area with bounded stdout.

```
:0 fw
| formail -I 'X-Checked: yes'
```

A complete action consisting only of `|` writes the selected area directly to
**procmail-rs** standard output without starting a shell.

## Command-output capture

`NAME=| COMMAND` assigns bounded stdout. Exactly one trailing LF is removed.
The `h` and `b` flags choose the input area, while `w`, `W`, `i`, and `r` retain
their pipe meanings.

```
:0 hW
MSG_FROM=| sed -n 's/^From: //p'
```

## Recipe blocks

A matched block evaluates its child statements in order. Chain state is local
to the block. A local lock named on the block is held across the complete child
sequence.

```
:0 : account.lock
{
    ACCOUNT=work

    :0
    * ^X-Account: work
    work/
}
```

The `c` flag on a block is not supported.

## Native header editing

The **procmail-rs** extension `headers { ... }` changes the current header
without starting an external filter. It is a recipe action, not a top-level
statement and not standard procmail syntax:

```
:0 [CONDITION_FLAGS] [CONTROL_FLAG]
* CONDITION
headers {
    OPERATION
    ...
}
```

The opening brace must occur on the action line after the word `headers`.
Blank and comment-only lines are accepted between operations. The closing `}`
must appear alone apart from whitespace. One action may contain at most 256
operations, and every physical operation line is checked against the active
`LINEBUF`.

Operations execute from top to bottom. A later operation sees the result of
every earlier one; this matters when `set`, `add`, and `remove` use the same
field name. Field-name comparisons ignore ASCII case, while field values are
never interpreted as addresses or MIME text.

`remove NAME`

: Delete every field named `NAME`. A field includes all immediately following
  physical lines beginning with space or tab, so removing a folded field also
  removes all its continuation lines. Absence of the field is success.

```
headers {
    remove X-Spam-Status
}
```

`set NAME: VALUE`

: Replace the first matching field at its existing position and remove every
  later duplicate. If no match exists, append one new field. The emitted name
  uses the spelling supplied by this operation, even when it replaces a field
  with different capitalization. `VALUE` may be empty.

```
headers {
    set X-Spam-Status: checked
    set X-Empty:
}
```

`add NAME: VALUE`

: Append one field after all fields currently present. Existing fields with
  that name are retained, so repeated `add` operations deliberately create
  repeated fields in source order.

```
headers {
    add Received: by first-filter
    add Received: by second-filter
}
```

`prepend NAME: VALUE`

: Insert one field before every field currently present. Consequently, a later
  `prepend` appears before an earlier `prepend`.

```
headers {
    prepend X-Processed-By: first
    prepend X-Processed-By: second
}
```

Here the resulting order is `second`, then `first`, then the original fields.

`NAME` must be non-empty printable ASCII without whitespace or colon. Punctuation
allowed by RFC-style field names is preserved. The first colon separates name
and value; later colons belong to `VALUE`:

```
headers {
    set X-Source-URI: imap://mail.example/inbox
}
```

Leading whitespace after the separating colon is removed by the rc parser.
New fields are serialized as `NAME: VALUE` followed by the line ending chosen
from the first physical input header line. If the input has no such line, the
header/body separator selects CRLF or LF; LF is the final fallback. Existing
unmodified fields, their original endings, malformed binary fields, the
separator, and the body retain their bytes.

`VALUE` supports `$NAME`, `${NAME}`, `${NAME:-expression}`, and the ordinary
backslash rules described under VARIABLE EXPANSION. Expansion occurs when the
selected recipe executes, so `MATCH`, numbered captures, `LASTFOLDER`, and
earlier assignments may be used:

```
:0
* ^List-Id:.*<\/([^.>]+)\.example>
headers {
    set X-List-Name: $MATCH1
}
```

Expanded bytes are inserted literally and are not scanned a second time.
Backquoted commands are not accepted in header-operation values. NUL, CR, LF,
and a trailing backslash requesting a folded continuation are rejected; native
editing cannot create a multi-line field.

The condition flags `H`, `B`, and `D`, control flags `A`, `a`, `E`, and `e`,
and `c` are accepted. A successful native edit always continues with the next
recipe, so `c` has no additional effect. Flags `h`, `b`, `f`, `w`, `W`, `i`,
and `r` are rejected because this action neither selects pipe input nor invokes
a child. A local lockfile is also rejected because no external publication
occurs.

Every operation is checked against `LIMIT_MSG_SIZE`, `LIMIT_MSG_HEADERS`,
`LIMIT_HEADER_LINE`, and `LIMIT_HEADER_FIELD`. All operations are prepared
privately; if parsing, expansion, validation, or any size check fails, none of
the changes becomes the current message and the preceding message remains
available to normal error handling. A successful header-only execution can
still stream an unread body without retaining it in memory.

For example, several common `formail -I` cleanup operations can be expressed
without copying the message through a child:

```
:0
headers {
    remove X-Spam-Status
    set X-Filter: checked
    add X-Filter-Result: clean
    prepend X-Processed-By: procmail-rs
}
```

This extension is not a general replacement for **formail**. It does not
extract or rename fields, parse addresses, generate `Message-Id`, split
digests, change the body, or implement `formail`'s duplicate-detection and
reply-header modes. Use a trusted pipe when those operations are required.

# VARIABLE REFERENCE

Names outside the typed names and unsupported list below are user variables.
They can be assigned in an rc file or supplied initially with **--set**. Values
may be arbitrary bounded bytes when produced by a capture, but a value exported
to a child environment cannot contain NUL. Assignment and expansion occur in
statement order.

## Identity and runtime results

`HOME`

: Initialized from the current uid's passwd entry. It is not imported from the
  ambient environment and may be replaced by an rc assignment or **--set**.

`LOGNAME`

: Initialized and controlled like `HOME` using the passwd login name.

`HOST`

: Initialized from the system node name. Assigning the exact system name
  continues processing. Any other value ends the current rc file successfully;
  a bare `HOST` is the supported empty assignment and therefore also ends it.

`PROCMAIL_VERSION`

: Read-only **procmail-rs** package version. It deliberately does not claim to
  contain the procmail 3.22 version.

`LASTFOLDER`

: Runtime-only path of the last successfully published destination. A failure
  before publication does not change it. When an error occurs after a message
  became visible, it records that visible destination.

`MATCH`

: Runtime-only bytes selected by the most recent successful `\/` regex marker.
  Evaluating another condition which contains `\/` or ordinary capture groups
  first clears all preceding match values, including when that condition fails.
  A regex without captures does not change them.

`MATCH1`, `MATCH2`, ...

: **procmail-rs** additions containing ordinary regex capture groups from the
  most recent successful regex condition. Unmatched optional groups are empty.
  These names cannot be assigned directly.

## Paths and delivery

`MAILDIR`

: Base directory for following relative destination, staging, lock, include,
  switch, and log paths. It has no implicit default; when unset or empty,
  relative paths use the process working directory. Assignment does not change
  the process working directory.

`DURABILITY`

: Select `none` (default), `file`, or `full`. `file` synchronizes delivered
  message data before success. `full` additionally synchronizes the documented
  publication directories. This is a statically prepared root setting rather
  than a message-conditional assignment.

`UMASK`

: Octal `0000` through `0777`, default `077`. It can only remove permission
  bits from restrictive modes requested for newly created delivery, lock, and
  log files. It never broadens permissions or changes the process-wide umask.

## Locking

`LOCKMETHOD`

: `flock` (default) selects a persistent checked file and kernel lock.
  `dotlock` selects compatible exclusive creation, stale removal, and final
  pathname removal with the documented replacement risk.

`LOCKFILE`

: Acquire a global lock at this statement. A later assignment releases the
  previous lock before acquiring its replacement; an empty value only releases
  it. The active `LOCKMETHOD`, `LOCKTIMEOUT`, `UMASK`, and `MAILDIR` apply.

`LOCKEXT`

: Suffix used to derive an implicit local lock name, default `.lock`. It may be
  empty but cannot contain NUL or `/`; both the suffix and resulting path are
  bounded.

`LOCKTIMEOUT`

: Finite lock acquisition limit in seconds, default 1024. Decimal values 1
  through 86400 are accepted. Zero is rejected rather than meaning an
  unlimited wait.

## External processes

`SHELL`

: Absolute shell path used for every trusted command, default `/bin/sh`. Empty,
  relative, trailing-slash, repeated-component, `.` and `..` paths are
  rejected. It may be supplied with **--set**.

`SHELLFLAGS`

: One argument placed between `SHELL` and command text, default `-c`. It is not
  split into several arguments and may be supplied with **--set**.

`PATH`

: Child command search path, default `/usr/bin:/bin`. It belongs to the rebuilt
  child environment and may be supplied with **--set**.

`TIMEOUT`

: Finite child limit in seconds, default 960. Decimal values 1 through 86400
  are accepted. It covers concurrent stdin writing, stdout reading, waiting,
  process-group termination, and reaping.

`TRAP`

: Bounded trusted shell command run after normal recipe completion and complete
  input validation. The final message plus one LF is supplied on stdin; stdout
  and stderr go to `LOGFILE`. An empty assignment disables a preceding trap.

`EXITCODE`

: Final decimal status from 0 through 255. An explicit non-empty value takes
  precedence. An explicitly empty value allows a nonzero `TRAP` result to
  become the final status; when never assigned, the provisional filter result
  remains authoritative and is exported to the trap.

## Diagnostics

`LOGFILE`

: Append destination for trace records, diagnostics, and child stderr. Empty or
  unset means diagnostics use standard error. The path is resolved through the
  active `MAILDIR`. This is a statically prepared root setting.

`VERBOSE`

: Enable or disable detailed tracing. Case-insensitive prefixes `on`, `y`, `t`,
  `e`, or a nonzero leading digit mean true; `off`, `n`, `f`, `d`, or a leading
  zero mean false. It is disabled by default and is statically prepared.

`LOGDETAIL`

: `metadata` (default) prevents variable values from entering trace records.
  `values` permits bounded and escaped value prefixes. Neither mode permits
  bodies, header values, regex text, destination paths, or command arguments.

`LOGABSTRACT`

: Only `no` is accepted. Original abstract modes are rejected because they log
  sensitive `From` and `Subject` values. No abstract is generated by default.

## Input and parser limits

`LINEBUF`

: Decimal limit for following physical rc lines, complete continued pipe
  commands, and values expanded by **procmail-rs**. It defaults to 2048 and may
  range from 128 through 1048576. Only a literal top-level assignment is
  accepted.

`LIMIT_MSG_SIZE`, `LIMIT_MSG_HEADERS`, `LIMIT_MSG_BODY`,
`LIMIT_HEADER_LINE`, `LIMIT_HEADER_FIELD`

: Bound complete mail, the header section, body, one physical header line, and
  one logical folded field. Values accept binary `K`, `M`, and `G` suffixes and
  cannot exceed the ceilings listed under LIMITS. These are statically prepared
  root settings.

`LIMIT_MAX_ASSIGNMENTS`, `LIMIT_RC_STATEMENTS`, `LIMIT_RC_RECIPES`,
`LIMIT_RC_CONDITIONS`, `LIMIT_RC_REGEXES`, `LIMIT_RECIPE_CONDITIONS`,
`LIMIT_RECIPE_NESTING`

: Decimal structural limits applied to following syntax. The assignment that
  changes a limit is checked using the preceding limit. These assignments and
  `LINEBUF` are rejected inside recipe blocks.

## Explicitly unsupported reserved names

Assignments and references to `DEFAULT`, `ORGMAIL`, `COMSAT`, `DELIVERED`,
`DROPPRIVS`, `LOCKSLEEP`, `LOG`, `MSGPREFIX`, `NORESRETRY`,
`PROCMAIL_OVERFLOW`, `SHELLMETAS`, `SUSPEND`, `SENDMAIL`, `SENDMAILFLAGS`, and
`SHIFT` are rejected with an explicit unsupported-variable diagnostic.
`LIMIT_RC_SIZE` is likewise reserved and rejected because an rc file cannot
choose the bound under which that same file was already read. These names are
not treated as ordinary user variables.

# RUNTIME RC FILES

`INCLUDERC=EXPRESSION` evaluates another rc file and then returns to the caller.
`SWITCHRC=EXPRESSION` evaluates another file and abandons the remainder of the
current file; an empty value ends only the current file.

```
:0
* ^List-Id:.*<\/[^.>]+\.([^.>]+)>
{
    INCLUDERC=$MAILDIR/rules/$MATCH1.rc
}
```

Runtime files must be regular, owned by the owner of the root rc file, neither
group- nor other-writable, and not reached through a final-component symlink.
Their number, total bytes, nesting depth, and transitions are bounded.

# LOCKING AND DURABILITY

A recipe colon requests a local lock. With no explicit name, a filesystem
recipe derives it by appending `LOCKEXT` to the resolved destination. An
explicit name is required for a pipe or block.

```
LOCKMETHOD=flock
LOCKTIMEOUT=30

:0 : archive.lock
mbox:archive
```

`LOCKMETHOD=flock` is the default and retains a checked lock file after use.
`LOCKMETHOD=dotlock` selects pathname-based procmail-compatible locking and its
documented replacement risk. `LOCKFILE` holds a global lock until it is
replaced, cleared, or processing ends.

`DURABILITY=none`, `file`, or `full` selects publication sync points. Maildir
uses an unnamed temporary inode and no-replace rename. Mbox holds `flock`,
records the original length, and attempts rollback after a failed append or
sync. No NFS-safety claim is made.

# LOGGING

`LOGFILE=PATH` selects append-only diagnostics. `VERBOSE=yes` enables detailed
metadata events without message contents, header values, paths, regex text,
commands, or variable values. `LOGDETAIL=values` additionally permits bounded,
escaped variable value prefixes. `LOGABSTRACT=no` is accepted; header-bearing
abstract modes are not.

```
LOGFILE=/srv/mail/filter.log
VERBOSE=yes
LOGDETAIL=values
```

Child stderr is appended to the active `LOGFILE` and is not discarded. A
trace-write failure is reported once to stderr and does not turn a successful
delivery into a retry that could duplicate mail.

# PROCESS SETTINGS AND TRAP

`SHELL`, `SHELLFLAGS`, and `PATH` construct the trusted child environment.
Ambient environment entries are not inherited. `TIMEOUT` bounds every external
shell; timeout terminates its process group with `SIGTERM` followed by
`SIGKILL` and reaps the direct child.

`TRAP` is the last executed non-empty trusted command assigned in statement
order. It runs after complete input validation and recipe processing, receives
the final message plus one LF, and writes stdout and stderr to `LOGFILE`.

```
TIMEOUT=60
TRAP=printf 'filter status=%s\n' "$EXITCODE"
```

The outer double quotes delimit the rc assignment word and are removed. The
single quotes remain inside that word and are later interpreted by the trusted
shell as part of the `TRAP` command.

# LIMITS

Message limits are `LIMIT_MSG_SIZE`, `LIMIT_MSG_HEADERS`, `LIMIT_MSG_BODY`,
`LIMIT_HEADER_LINE`, and `LIMIT_HEADER_FIELD`. Their byte counts accept an
optional binary `K`, `M`, or `G` suffix.

| Variable | Default | Hard ceiling |
| --- | ---: | ---: |
| `LIMIT_MSG_SIZE` | 64 MiB | 256 MiB |
| `LIMIT_MSG_HEADERS` | 256 KiB | 16 MiB |
| `LIMIT_MSG_BODY` | 64 MiB | 256 MiB |
| `LIMIT_HEADER_LINE` | 64 KiB | 1 MiB |
| `LIMIT_HEADER_FIELD` | 256 KiB | 16 MiB |

`LIMIT_MSG_SIZE`

: Bounds the complete message in bytes: the original header bytes, the
  header/body separator when present, and the body. It is checked while stdin
  is read and again when a filter or native header edit produces a replacement
  message. A body may be streamed instead of retained, but its bytes still
  contribute to this limit.

`LIMIT_MSG_HEADERS`

: Bounds the complete raw header section in bytes, including physical line
  endings and the empty line which separates headers from the body. If EOF is
  reached before an empty separator line, every byte read is treated as header
  data and contributes to this limit.

`LIMIT_MSG_BODY`

: Bounds the bytes after the first empty header line. The separator itself is
  not body data. A message without a separator therefore has a zero-byte body.
  The limit is enforced for retained and streamed bodies and for replacement
  messages returned by filters.

`LIMIT_HEADER_LINE`

: Bounds one physical header line, including its LF or CRLF ending. A final
  unterminated line is also checked. Folded continuations are separate physical
  lines for this limit. Reading stops as soon as the next input chunk proves
  that the line would be too large; EOF is not required.

`LIMIT_HEADER_FIELD`

: Bounds one logical header field before folding is normalized for matching.
  The count is the sum of the original bytes in its first physical line and
  every immediately following line beginning with space or tab, including
  their line endings. A continuation cannot evade the physical-line limit:
  both limits apply.

Structural limits are `LIMIT_MAX_ASSIGNMENTS`, `LIMIT_RC_STATEMENTS`,
`LIMIT_RC_RECIPES`, `LIMIT_RC_CONDITIONS`, `LIMIT_RC_REGEXES`,
`LIMIT_RECIPE_CONDITIONS`, and `LIMIT_RECIPE_NESTING`. They accept decimal
counts and cannot exceed their compiled ceilings.

| Variable | Default | Hard ceiling |
| --- | ---: | ---: |
| `LIMIT_MAX_ASSIGNMENTS` | 4096 | 65536 |
| `LIMIT_RC_STATEMENTS` | 4096 | 65536 |
| `LIMIT_RC_RECIPES` | 1024 | 16384 |
| `LIMIT_RC_CONDITIONS` | 4096 | 65536 |
| `LIMIT_RC_REGEXES` | 256 | 1024 |
| `LIMIT_RECIPE_CONDITIONS` | 256 | 4096 |
| `LIMIT_RECIPE_NESTING` | 64 | 256 |

`LIMIT_MAX_ASSIGNMENTS`

: Bounds assignments across the root rc file and every runtime `INCLUDERC` or
  `SWITCHRC` file. Ordinary assignment statements count. A `NAME=| command`
  recipe action also counts because it writes command output to a variable.
  The assignment which changes this limit is charged against the preceding
  value, then its new value applies to following source text.

`LIMIT_RC_STATEMENTS`

: Bounds top-level and nested assignment and recipe statements across all rc
  files loaded for one message. Blank lines, comments, condition lines, action
  lines, and header-edit commands are not separate statements. An assignment
  that is also counted by `LIMIT_MAX_ASSIGNMENTS` consumes one statement;
  `NAME=| command` consumes an assignment but not an additional statement
  beyond its containing recipe.

`LIMIT_RC_RECIPES`

: Bounds recipe headers beginning with `:0` across all loaded rc files. A
  recipe whose action is a block counts once, and every recipe inside that
  block counts separately.

`LIMIT_RC_CONDITIONS`

: Bounds all recipe condition lines beginning with `*` across all loaded rc
  files. Regex, size, program, and shell-expanded `$` conditions each consume
  one entry.

`LIMIT_RC_REGEXES`

: Bounds conditions which can compile a regular expression. An ordinary regex
  condition counts once. A shell-expanded `$` condition also reserves one
  entry because its expanded text is reparsed at runtime and may become a
  regex. Size and program conditions do not count. This limits the number of
  regex programs, not the number of capture groups; those have separate fixed
  ceilings.

`LIMIT_RECIPE_CONDITIONS`

: Bounds condition lines in one recipe. The count starts again for each nested
  or following recipe. The same condition is also charged to the file-wide
  `LIMIT_RC_CONDITIONS` count.

`LIMIT_RECIPE_NESTING`

: Bounds nested recipe-action blocks. A root-level recipe is at depth zero;
  entering its `{ ... }` action creates depth one. Plain recipe count is
  controlled separately by `LIMIT_RC_RECIPES`.

```
LIMIT_MSG_SIZE=32M
LIMIT_MSG_HEADERS=512K
LIMIT_HEADER_LINE=64K
LIMIT_RC_RECIPES=2048
LINEBUF=4096
```

All message and count limits accept zero. A limit permits exactly that many
bytes or items and rejects the next one; values at `limit - 1` and `limit`
succeed when no other limit is exceeded. Structural limits and `LINEBUF` take
effect in statement order. Lowering a structural limit below work already
counted is accepted; the next matching item is rejected. Structural-limit and
`LINEBUF` assignments are rejected inside recipe blocks. Runtime rc files share
the counts accumulated by their caller and see only settings which executed
before they were loaded.

`LIMIT_RC_SIZE` is reserved but not supported because an rc file cannot safely
choose the limit used to read itself. Each rc file instead has a fixed 1 MiB
ceiling, and all rc files loaded while processing one message have a fixed
combined 4 MiB ceiling. See **Documentation/Limits.md** for the other fixed
ceilings which have no rc variable.

`TIMEOUT` and `LOCKTIMEOUT` accept 1 through 86400 seconds; zero is rejected.
`UMASK` accepts octal `0000` through `0777` but can only remove permissions
from restrictive backend modes.

# PRACTICAL EXAMPLES

The destination Maildirs used below, including their `tmp`, `new`, and `cur`
subdirectories, must be created before filtering.

## Mailing-list delivery

Capture two components of `List-Id`, reverse them in the destination hierarchy,
and retain a final inbox:

```
MAILDIR=/srv/mail

:0
* ^List-Id:.*<\/([^.>]+)\.([^.>]+)>
lists/$MATCH2/$MATCH1/

:0
inbox/
```

## Filtering and native header cleanup

Remove stale scanner results without `formail`, run a spam filter over the
complete message, then classify its generated header. A failed waited filter
sets status 75 and stops the current rc file through `HOST`.

```
:0
headers {
    remove X-Spam-Flag
    remove X-Spam-Level
    remove X-Spam-Status
}

:0 fw
| bogofilter -p -u -e -c "$HOME/.bogofilter.cf"

:0 e
{
    EXITCODE=75
    HOST
}

:0
* ^X-Bogosity: (Spam|Yes)
spam/

:0
inbox/
```

## Bounded command-selected folder

Backquoted destination fragments receive the complete message, have trailing
LF bytes removed, and are checked as part of the final path. The selected
year-month Maildir must already exist.

```
MAILDIR=/srv/mail/archive

:0
maildir:`date +%Y-%m`
```

## Header extraction for later rules

Capture a header through a trusted external command when native modification is
not enough. `h` avoids sending the body and `W` observes failure quietly.

```
:0 hW
MSG_TO=| sed -n 's/^To:[[:space:]]*//p'

:0
* MSG_TO ?? user@example\.org
personal/
```

## Message-selected rule file

An included file can depend on a capture made while filtering. It is loaded
only on the selected path, then processing returns after its statements end.

```
RULE_ROOT=/srv/mail/rules

:0
* ^X-Account: \/[A-Za-z0-9_-]+
{
    INCLUDERC=$RULE_ROOT/$MATCH.rc
}
```

# UNSUPPORTED FEATURES

There is no implicit `DEFAULT` or `ORGMAIL` delivery, system mailbox, sendmail
forward action, comsat notification, privileged identity change, LMTP mode, MH
folder, ordinary directory folder, multi-folder action, weighted scoring, or
automatic `~/.procmailrc` discovery.

Reserved names for unsupported procmail behavior are rejected explicitly.
Consult `Documentation/Compatibility.md` for narrowed semantics and known
regular-expression differences before using an existing procmail rc file.

# SEE ALSO

**procmail-rs**(1), **maildir**(5), **mbox**(5)

The source distribution also contains `Documentation/Compatibility.md`,
`Documentation/Delivery.md`, and `Documentation/Limits.md`.
