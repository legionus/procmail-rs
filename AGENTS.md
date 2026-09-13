# procmail-rs development rules

These instructions apply to the entire repository.

## Project scope

`procmail-rs` is a mail filtering program. It reads one message from standard
input, evaluates a deliberately limited subset of procmail recipes, and will
deliver the message only to an explicitly selected mbox or Maildir target, or
discard it through an explicitly selected `/dev/null` destination.

The currently supported build platforms are 32-bit and 64-bit Unix systems.
Linux uses its dedicated unnamed-file Maildir backend; other Unix systems use
the portable named-file backend. Linux and FreeBSD have native runtime tests.
Cross-compilation for other systems verifies API availability but does not
establish filesystem, mapping, locking, durability, or process behavior there.
Both pointer widths require dedicated mapping and address-space failure tests.

Do not add local-delivery-agent behavior, implicit delivery to
`/var/spool/mail/$LOGNAME`, passwd-based recipient lookup, setuid/setgid,
LMTP, comsat, forwarding through sendmail, or an implicit fallback mailbox
unless the project scope is explicitly changed first.

The sources under `external/procmail-3.22/` are a behavioral reference. Do
not copy their architecture or unsafe assumptions mechanically. Document
intentional compatibility differences and cover supported behavior with
tests.

## Security priority

Security is the first design constraint, not a cleanup phase after a feature
works. Before changing a parser, evaluator, process runner, logger, or delivery
backend, identify which hostile inputs reach it, which resources can grow, and
which filesystem or process state can change concurrently. Do not implement a
feature until its bounds, failure behavior, and publication point are clear.

Never weaken input validation, resource limits, pathname handling, process
supervision, data-hiding defaults, or delivery checks merely to simplify an
implementation or imitate original procmail. When compatibility requires a
weaker path, stop and discuss the specific risk with the user, isolate the
behavior behind an explicit setting or platform backend, document it, and add
focused tests. An unsupported construct with a precise diagnostic is safer
than a partial interpretation.

Errors must fail closed before message delivery. Do not continue with a
default value, truncated data, partially parsed syntax, an inferred
destination, or a best-effort publication unless that exact recovery behavior
has been reviewed and documented. Preserve enough context to diagnose the
failure without exposing message data or private configuration values.
An unresolved security question blocks completion: stop, describe the risk,
and ask the user before weakening a safety boundary.

## Threat model

Treat every external input as hostile, malformed, excessively large, slow,
or non-terminating. This includes:

- the message read from stdin;
- headers, continuation lines, and the body;
- rc files, assignments, recipes, paths, and regular expressions;
- environment variables and command-line arguments;
- filenames, directories, mailbox contents, filesystem metadata, locks, and
  concurrent filesystem changes;
- data returned by child processes if process execution is ever supported.

Never assume that an input is UTF-8 unless its interface explicitly requires
UTF-8. Mail messages are byte sequences and must preserve arbitrary bytes.

An API that accepts an already unbounded allocation is not an adequate
substitute for bounded ingestion. Enforce limits while reading and stop as
soon as a limit is known to be exceeded. A missing newline and a missing EOF
must not allow unbounded memory use or indefinite accumulation.

Use checked arithmetic for sizes, offsets, counters, capacities, timestamps,
and retry calculations derived from input. Reject overflow explicitly.

## Resource limits

All potentially growing input structures need finite limits. Limits must
cover aggregate size and any smaller unit that can independently grow without
bound. For mail input this currently includes:

- total message size;
- total header section size;
- body size;
- one physical header line;
- one logical header field including folded continuation lines.

The rc variables `LIMIT_MSG_SIZE`, `LIMIT_MSG_HEADERS`, `LIMIT_MSG_BODY`,
`LIMIT_HEADER_LINE`, and `LIMIT_HEADER_FIELD` may select limits within hard
ceilings. Configuration must not raise a hard ceiling. The rc file itself
must be bounded before it is parsed because it cannot safely configure its
own read limit.

Structural parser limits use the same two-level policy. The variables
`LIMIT_MAX_ASSIGNMENTS`, `LIMIT_RC_STATEMENTS`, `LIMIT_RC_RECIPES`,
`LIMIT_RC_CONDITIONS`, `LIMIT_RC_REGEXES`, `LIMIT_RECIPE_CONDITIONS`, and
`LIMIT_RECIPE_NESTING` change their active values in statement order and may
not exceed their hard ceilings. Check each assignment using the preceding
assignment limit, then apply its new value only to following syntax. Lowering
a limit below its current count is allowed; reject the next matching item.
Reject these parser-limit assignments inside recipe blocks. Apply values that
have actually executed before a runtime `INCLUDERC` or `SWITCHRC`; a later
assignment must not affect an earlier include. `LIMIT_RC_SIZE` is deliberately
unsupported because the file must be bounded before any assignment is read.

`LINEBUF` defaults to 2048 and may select a literal decimal limit from 128
through `MAX_RC_SIZE`. Apply it to following physical rc lines, complete
continued pipe commands, and values expanded by procmail-rs. Keep message
header lines and trace records under their independent limits. Reject
`LINEBUF` inside recipe blocks and reject non-literal values because parsing
finishes before message-dependent execution begins.

New collections, nesting, regex patterns, substitutions, includes, process
output, retries, and concurrency require explicit limits before the feature
is accepted. Prefer a clear rejection to truncation or partial interpretation.

Native header extraction is limited to `MAX_ASSIGNMENT_VALUE_LEN` bytes and
may assign only ordinary user variables. Raw extraction must check its selected
range before copying; unfolded extraction must enforce the limit during
construction. Hold all extracted values privately until the complete header
action and every message-size check have succeeded.
The 256-operation action limit and 65536-byte value limit cap the aggregate
retained extraction output at 16 MiB.

Test every new limit at `limit - 1`, `limit`, and `limit + 1` where applicable.
Also test unterminated and non-terminating input with a reader that proves the
implementation stops without waiting for EOF.

## Rust safety

Unsafe Rust is a project blocker except for the narrowly reviewed read-only
mapping implementation in `src/mapped_file.rs`, current-user passwd lookup in
`src/user_identity.rs`, and signal-handler installation in
`src/signal_state.rs`. Executable targets must
declare:

```rust
#![forbid(unsafe_code)]
```

The library uses `#![deny(unsafe_code)]` and permits unsafe code only on the
private `mapped_file` module and the public identity and signal modules. The
mapping module may call `rustix::mm` to create and release a read-only mapping
and to expose its bytes as a slice. The identity module may call only `getuid`
and `getpwuid_r`, copy `pw_name` and `pw_dir` from a bounded caller-owned
buffer, and reject pointers outside that buffer. The signal module may use
`libc::sigaction` and `libc::sigemptyset` only to install handlers for
`SIGHUP`, `SIGINT`, `SIGQUIT`, and `SIGTERM`; handlers may only record the
first signal in a lock-free atomic. Do not add another unsafe operation,
expand these modules' responsibilities, or increase their unsafe counts
without explicit user approval and a focused review. Do not hide unsafe code
behind generated code or introduce unrelated FFI as a workaround.

Unsafe code in a dependency is not automatically acceptable. It requires an
explicit discussion of:

- why the dependency is needed;
- where and why it uses unsafe code;
- which unsafe code is active under selected features;
- whether features can reduce the unsafe surface;
- safe alternatives and their correctness or performance costs;
- the exact compromise being accepted.

The current `regex` dependency is an explicit exception. Only `std` and
`perf-literal` are enabled. This preserves practical scanning performance
while avoiding the broader default DFA, backtracking, one-pass, cache,
inline, and Unicode feature set. Do not change its version or features
without repeating the dependency and performance review.

The direct `regex-syntax` dependency is an explicit exception for parsing the
source expression into an upstream-maintained AST, using typed nodes and source
spans to locate procmail-specific forms before compilation.
Version 0.8.11 was already present through `regex`; making it direct adds no
crate or version to the dependency tree. Keep default features disabled and
enable only `std`, which was already active. The crate reports no active unsafe
code, uses heap-backed traversal for hostile nesting, and is licensed under
`MIT OR Apache-2.0`. Do not change its version or features without repeating
the dependency and unsafe review.

The `rustix` dependency is an explicit exception for race-resistant Maildir
filesystem operations, fallible random Maildir names, the isolated read-only
mapping implementation, signalling supervised external process groups, and
safe current-hostname lookup. Keep default features disabled and enable only
`fs`, `mm`, `process`, `rand`, and `system`. It supplies typed
directory-relative filesystem, random, mapping, process, and system APIs while
containing unsafe Linux syscall and ABI bindings internally. Enabling `process`
raised active rustix audit counts from 69 functions and 822 expressions to 81
functions and 1222 expressions; `system` then raised the expression count to
1242. The project-owned count remained 18 expressions. Do not change its
version, features, or backend without repeating the dependency and unsafe review.

The direct `libc` dependency is an explicit exception for `getuid` and
`getpwuid_r` in `user_identity.rs` and signal-handler installation in
`signal_state.rs`. It preserves NSS behavior that cannot be obtained by
parsing `/etc/passwd`; the project-owned identity wrapper bounds the caller
buffer and validates returned field pointers and text. The signal wrapper uses
only `sigaction` and `sigemptyset`, and its handler performs only a lock-free
atomic update. Do not use libc elsewhere without another explicit discussion
and focused review.

Use `cargo geiger` to audit the active dependency tree. A textual search for
the word `unsafe` is not a substitute for that audit.

The reviewed `mapped_file` implementation contains three unsafe blocks. With
the current geiger version these are reported as ten unsafe expressions. Treat
any increase in either the source block count or the geiger expression count as
a review failure unless the user explicitly approves the change.

The reviewed `user_identity` implementation contains three unsafe blocks for
`getuid`, `getpwuid_r`, and reading the successfully initialized passwd record.
They add eight geiger expressions, bringing the project total from ten to
eighteen. Treat three source blocks and eight audit expressions as fixed
ceilings for this module until another focused review is approved.

The reviewed `signal_state` implementation contains one unsafe block for
initializing and installing the four signal handlers. With the current geiger
version it adds twenty-five unsafe expressions, bringing the project total
from eighteen to forty-three. Treat one source block and twenty-five audit
expressions as fixed ceilings for this module until another focused review is
approved.

## Dependencies and tools

Do not add a crate, enable a new crate feature, or replace an implementation
with a third-party module without first discussing the need and tradeoffs
with the user. Consider the standard library, maintenance status, security
history, transitive dependencies, feature set, unsafe usage, and resource
behavior.

`cargo add`, `cargo update`, and equivalent dependency mutations require that
discussion and approval. Keep `Cargo.lock` committed and use `--locked` for
verification.

If a tool required for implementation or prescribed verification is missing
or fails to run, stop and ask the user to install or repair it. Do not silently
skip the check or substitute an ad-hoc approximation.

## Development workflow and shared design

The project is not published as a Rust library. Do not preserve an obsolete
public or private API solely to avoid updating callers inside this repository.
Prefer the simplest final design and update all callers, tests, and
documentation in the same change.

Before adding a type, helper, parser, evaluator, renderer, platform operation,
or test utility, search the repository with `rg` for existing and closely
related behavior. Read the full relevant code path, its tests, and the nearby
documentation before editing it. Treat a similar implementation as a signal
to centralize the behavior, not as a template to copy into another location.

Maintain one authoritative implementation for each behavior. When two paths
perform the same parsing, expansion, validation, limit accounting, error
classification, rendering, or state transition, refactor the shared operation
into one appropriately owned type or module and route both callers through it.
Do not keep compatibility wrappers, legacy entry points, test-only copies, or
parallel implementations after their callers can use the shared path. Platform
modules may differ only where operating-system behavior actually differs and
must expose the same small high-level API to common code.

Do not combine unrelated cleanup with a behavioral change. If safe feature
work first requires a substantial consolidation, make that consolidation an
independent step with behavior-preserving tests, then implement the feature on
the shared design. For work with several independently reviewable steps,
record or confirm the order first, complete one focused step at a time, run
narrow checks after each step, and leave a clear diff boundary for each future
commit. Update `TODO.md` when a multi-step plan must survive beyond the current
context.

Keep tests outside production source bodies when practical. Small private
unit hooks may remain next to the code, but substantial suites belong under
`src/tests/` or top-level `tests/` and should be split by behavior rather than
accumulated in one large file. Tests must call the production path instead of
reimplementing its logic to compute expected results.

Inspect the worktree before editing. Preserve unrelated user changes and do
not stage them. Do not commit, amend, rebase, push, or publish unless the user
explicitly requests that operation. A request for a commit message does not
authorize creating the commit.

## Parsing and evaluation

Keep syntax and semantics explicit in the parser and typed AST. Reject an
unsupported procmail construct with a source location and a specific error;
never reinterpret it as a path, assignment, or supported action.

Compile and validate configuration, limits, and regular expressions before
reading stdin. Invalid configuration must not consume or deliver a message.

Do not import ambient process environment variables into rc expansion. Obtain
the initial `HOME` and `LOGNAME` values from the current uid through the
bounded passwd lookup, and admit other external values only through the
policy-checked `--set` interface. Rc assignments may replace these initial
values. Tests must show that ambient values, including ambient `HOME` and
`LOGNAME`, cannot override the passwd result and that every other environment
name remains unavailable.

Variable references use a deliberately limited shell-like syntax. Support
`$NAME`, `${NAME}`, `$N`, `${N}`, `$#`, `${#}`, `${NAME-word}`, `${NAME:-word}`, `${NAME+word}`,
`${NAME:+word}`, `${NAME:=word}`, `${NAME:?word}`, `${#NAME}`, and the pattern
removal and ASCII case forms described below, where `NAME` starts with an ASCII
letter or underscore and continues with ASCII letters, digits, or underscores.
The unbraced form
consumes the longest valid name, so braces are required to separate a name
from adjacent name characters. The `-` forms select `word` when the name is
unset, while `:-` also treats an empty value as unset. The `+` forms select
`word` when the name is set, while `:+` additionally requires a non-empty
value. Permit an empty `word` and evaluate a selected word lazily. Expand
references in statement order and insert each selected value literally
without scanning the inserted bytes again. `${NAME:=word}` may assign only an
ordinary user variable. Stage its changes until the complete expression has
succeeded, make staged values visible to later parts of that expression, and
discard every staged change on an expression error. Support it only in
assignment values and destination expressions; reject it during configuration
preparation in contexts without sequential mutable evaluation. `${NAME:?word}`
must report only that `NAME` is unset or empty. Parse `word`, but never evaluate
it or expose it in the diagnostic. Outside double quotes, a backslash
makes any following character literal and is itself removed. Inside double
quotes, it does this only for `$`, backquote, `"`, backslash, and newline;
before another character the backslash remains literal. Assignment values are
one shell-like word and may concatenate unquoted, single-quoted, and
double-quoted fragments. Single quotes make every enclosed byte literal;
variable references, backquoted commands, backslashes, and double quotes have
no special meaning until the closing single quote. Remove quote delimiters and
reject a second unquoted word instead of silently ignoring it. An unquoted `#`
starts a comment only at a word boundary; inside a word or either quote mode it
is data. This does not authorize general shell execution, field splitting,
globbing, tilde expansion, arithmetic substitution, `$@`, `$*`, or
other parameter operators. Reject unsupported forms explicitly rather than
passing them to a shell or assigning them a new meaning.

A bare variable name removes that variable, while `NAME=` retains an explicitly
empty value. Preserve this distinction through selected blocks and runtime rc
files. Removing a setting with a documented default restores that default for
later operations. Apply parser-setting removal in source order, charge it
against the preceding assignment limit, and reject it inside recipe blocks.
Keep bare `HOST` as its documented control statement rather than treating it as
a removal.

`SHIFT` accepts a positive decimal integer and advances the positional
argument window in statement order, capped at the number of remaining
arguments. Following `$N` and `$#` references must observe the shifted window.
Keep the shift local to a copied recipe branch and preserve it across runtime
include and switch boundaries.

`${#NAME}` reports the byte length of the value. `${NAME#pattern}` and
`${NAME##pattern}` remove the shortest or longest matching prefix;
`${NAME%pattern}` and `${NAME%%pattern}` do the same for a suffix. Patterns
operate on bytes and support `*`, `?`, byte classes, byte ranges, leading `!`
or `^` class negation, and backslash quoting. Quoted pattern fragments are
literal. Unquoted variable and command output contributes active pattern
syntax, while quoted output is escaped before matching. Bound pattern
evaluation by the enclosing expression limit and reject work above the fixed
shell-pattern step ceiling before matching begins.

`${NAME^pattern}` and `${NAME,pattern}` change the first byte to ASCII upper or
lower case when it matches; the `^^` and `,,` forms consider every byte. An
empty pattern defaults to `?`. These operations do not consult locale and
preserve non-ASCII bytes. Apply the same pattern work ceiling before changing
the value.

Regular expression matching operates on bytes. Preserve the documented
header/body selection semantics. The single supported dialect is the Rust
byte-regex syntax accepted by `regex-syntax`, augmented with the documented
procmail anchors, word edges, capture marker, and reserved macros. Record forms
whose meaning differs from original procmail in the compatibility document;
do not add parallel parser modes to hide those differences.

Destination type must come entirely from rc syntax. Accept `maildir:PATH` and
paths ending in `/` as Maildir, and accept `mbox:PATH` as mbox. Unlike
procmail, do not inspect whether an unmarked path currently names a directory;
reject it as ambiguous so a concurrent filesystem change cannot alter the
delivery backend.

Filesystem paths supplied by rc or `--set` are UTF-8. Permit absolute paths
and resolve relative paths against the `MAILDIR` active at that statement, or
against the process working directory when no `MAILDIR` is set. Reject empty
paths, NUL, `.` and `..` components, repeated separators, and trailing `/`
outside Maildir syntax before reading stdin. Do not use `stat` to approve a
path. Maildir access must open every component relative to an already opened
directory with `NOFOLLOW`, including `tmp` and `new`, so symlink changes are
checked by the operation that actually obtains each descriptor.

Keep destination paths as bounded expressions until their recipe is selected.
Apply rc assignments to the runtime value table in statement order. Bind
ordinary variables when a recipe is selected so later assignments cannot
change an already selected copy, but leave runtime-produced values such as
`LASTFOLDER` unresolved until the destination is opened. If a destination
depends on a previous publication, stage the complete message and publish the
selected destinations in order.

No message may be considered delivered until a complete destination operation
has succeeded. Copy-only recipes must not mark the original as delivered.

## Filesystem delivery

Filesystem paths and metadata are hostile and may change concurrently. Avoid
time-of-check/time-of-use assumptions, unintended symlink traversal, path
ambiguity, predictable temporary-file replacement, and partial delivery.

Local recipe locks use `LOCKMETHOD=flock` by default and retain their checked
regular file after releasing the kernel lock. `LOCKMETHOD=dotlock` is an
explicit compatibility exception: creation is exclusive, but stale removal
and final cleanup use pathname-based `unlink` like original procmail and can
remove a substituted entry. Keep that risk isolated in `local_lock.rs`, make
it opt-in from the rc file, and document it wherever dotlock behavior is
described.

`LOCKTIMEOUT` accepts only 1 through 86400 seconds. Do not restore original
procmail's zero-means-infinite behavior. Apply the active value to global and
recipe lock acquisition and to mbox flock acquisition; an assignment affects
only later attempts. `LOCKFILE` replaces and releases the preceding global
lock in statement order and an empty value releases it without acquiring a
replacement.

`TIMEOUT` defaults to 960 seconds and accepts only 1 through 86400. Every
external shell must run in its own process group while timeout supervision is
active concurrently with pipe input and output. On expiration send `SIGTERM`,
wait at most 250 ms, send `SIGKILL`, and reap the direct child. Preserve the
existing `w`, `W`, `i`, and `e` status behavior. A process that deliberately
leaves the group is outside this mechanism; do not claim cgroup-style
containment.

`UMASK` defaults to octal `077` and accepts only `0000` through `0777`. Apply
it in statement order to user-visible files created for delivery, locking,
and logging by clearing bits from each backend's restrictive requested mode.
Never use it to grant group or other access, chmod existing files, weaken
private staging, or mutate the process-wide umask.

`TRAP` is a bounded trusted-shell command evaluated in statement order. A
reachable assignment requires replayable private staging, but the completion
path must expose one borrowed final-message view over mapped staging or the
owned filter replacement rather than copying a large message. Run it only
after complete-input validation and recipe processing, never for `check`,
including `check --explain`, partial input, or signal termination. Append both
stdout and stderr
to `LOGFILE`, apply the active `TIMEOUT`, and preserve the documented unset,
empty, and explicit `EXITCODE` behavior.

Maildir delivery must use exclusive temporary-file creation, complete and
checked writes, and an atomic move from `tmp` to `new`. Clean up owned
temporary files on ordinary failure without deleting paths that may belong to
another process.

Mbox delivery requires an explicitly documented interprocess locking policy,
complete-write handling, rollback after partial append when possible, and
tests with concurrent writers. Do not claim NFS safety without dedicated
integration tests.

Durability guarantees such as file and directory `fsync` must be explicit and
tested. Do not report successful delivery before the configured durability
point has been reached.

## Error handling and observability

Do not use `unwrap`, `expect`, or `panic` on a path reachable from hostile
runtime input. They are acceptable in tests when a failure should abort the
test.

Distinguish configuration errors, rejected input, transient delivery errors,
permanent destination errors, and internal failures. Include the relevant rc
path and source line for configuration errors and the exact limit name for
limit failures. Never log message bodies, credentials, or sensitive header
values by default.

Do not silently discard a message, truncate it, or fall back to an implicit
destination.

Trace log failures are advisory. A logger must report the first failure once
to stderr and then disable further log writes for that message. It must not
change a successful delivery into failure because an MTA retry could publish a
duplicate message. Delivery and input failures still determine the filtering
result independently of trace output.

The default trace is metadata-only. Its event types and renderer must not
contain message bodies, header values, regular expression text, destination
paths, variable values, credentials, or external command arguments. Variable
names, rc source lines, typed decisions, and result categories are allowed.
Test the final rendered bytes with distinct sentinel values in every excluded
source whenever the event schema or renderer changes.

Variable values may appear only when the rc file explicitly sets
`LOGDETAIL=values`; `VERBOSE` alone is insufficient. The metadata mode is the
default. High-detail values must pass through the same escaping and record
limits, and each value is restricted to a bounded prefix with an explicit
truncation marker. This mode never permits message bodies or header values.

## Code comments and terminology

Add a short explanatory comment before a sufficiently complex group of
related operations. The comment should explain why the group is needed, which
safety or correctness guarantee it preserves, why a simpler-looking approach
would be insufficient, and which assumptions must remain true when the code is
changed. Do not merely restate the operations performed by the code.

Avoid the words commonly used for a property that must always remain true and
for a formal agreement between an API and its callers. Prefer direct wording
such as "guarantee", "required condition", "assumption", "API behavior", or
"caller requirement" instead.

## Tests and verification

Every behavioral change needs focused tests. Security boundaries require
adversarial tests, including malformed binary input, missing delimiters,
integer boundaries, infinite readers, filesystem races where practical, and
concurrent delivery.

Run the narrowest relevant test while developing so failures remain local and
diagnosable, then run the complete required checks before handoff. A failure
must be investigated; do not label it flaky or retry until it passes without
understanding whether the code or test contains a race. When changing a parser
or expansion engine, add regression cases for the reported input and boundary
cases around it. When changing filesystem or process behavior, exercise
failure cleanup and concurrency where the host can do so safely.

Before handing off changes, run at least:

```text
cargo fmt -- --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo geiger --locked --forbid-only
git diff --check
```

Run the full `cargo geiger --locked` report when a dependency, dependency
feature, or compiled target changes. Run the regex benchmark when changing
the regex version or features. Use release builds for performance comparisons
and compare identical workloads over multiple rounds.

Run `make check-man` whenever editable manual sources, their generator, or
generated pages change. Compile-check every affected cross target when shared
Unix or pointer-width-dependent code changes, and run native platform tests
where the required runner exists. Cross-compilation establishes API
availability only and must not be reported as runtime validation.

Before handoff, inspect `git status`, review the complete relevant diff, and
run `git diff --check`. Report which checks passed and any check that could not
be completed. Do not broaden a claim beyond what those checks exercised or
describe hygiene checks alone as proof of semantic correctness, delivery
atomicity, durability, or concurrency safety.
