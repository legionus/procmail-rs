# Message copying and matching preparation

Run the repeatable message benchmark with:

```sh
cargo run --locked --release --example message-bench
cargo run --locked --release --example message-bench -- path/to/config
```

The fixed input is a 1 MiB message with a folded Subject. Each primitive
benchmark runs 100 iterations in five rounds. Full-filter measurements include
cloning the input to simulate a newly owned filter result. Matching measurements
compare preparation with and without a full normalized view.

An optional rc file is read through the bounded rc loader and compiled once.
The replay evaluates it 50 times per round. HOME and LOGNAME have fixed benchmark
values. Filters return their selected input unchanged, captures return
`mock`, and program conditions return false. Delivery only observes the bytes
and updates LASTFOLDER; logging and locks are disabled. Includes still use the
normal bounded loader, so configuration paths must be readable.

This measures evaluator work under deterministic command outcomes. It does not
measure real spam filtering, shell execution, filesystem publication, or
durability. A different message or command result can select other recipes.

## Measurements on 2026-10-08

Release builds used opt-level 3 and fat LTO on the same Linux x86_64 host.
The baseline was commit `f40a856`. Both versions used the same benchmark,
with only the changed ownership of the filter-output argument adapted.

| Operation | Baseline median | New version median |
| --- | ---: | ---: |
| Full filter result, 1 MiB | 0.866 ms | 0.0246 ms |
| Local migration rc replay, 1 MiB | 1.614 ms | 1.453 ms |

The migration configuration was the local `procmailrc`. Its two literal
multiline LOG assignments were put on one line in a temporary benchmark copy
because the parser rejects those assignments. The original file was untouched.
The same temporary copy was used for both versions.

The full-filter result removes an extra message-sized allocation and body
copy, while still checking every active limit through the existing bounded
streaming reader. The rc replay varied from 1.498 to 1.784 ms before the change
and from 1.381 to 1.540 ms afterward. Those overlapping ranges do not establish
a reliable end-to-end speedup.

## Allocation behavior

Full-message filters retain their owned Message after successful validation.
Header-only and body-only external filters still build a bounded replacement
containing the changed area and the preserved area. Native header changes keep
their new headers separately and share the existing mapped or filter-owned
body. Extraction-only actions do not create a new message version when the
header bytes remain unchanged.

Each replacement owns an immutable version shared with copied recipe branches.
Its full normalized regex view is prepared only when a resolved HB condition
actually reaches matching, then reused by later conditions and shared branches.
A subsequent filter or header change creates a new version with an empty
cache. Headers, body-only regexes, captures, delivery, and TRAP borrow their
appropriate views without forcing that cache. Delivery, shell command input
and TRAP write borrowed message parts sequentially. Mbox quoting handles lines
that cross the boundary without copying or buffering a complete physical line.

Original folded input uses a separate mapped matching file when the compiled
plan needs full matching. Raw message access for an external command does not
itself request that file. Runtime includes/switches and shell-expanded
conditions remain conservative: they can introduce HB regexes that were not
known before execution. Keeping the original normalized view available prevents
those conditions from silently matching raw folded headers instead.

## Peak memory baseline on 2026-10-08

Build release binaries before measuring. The memory benchmark accepts explicit
paths to the Rust binary and an independently built procmail/formail pair:

```sh
cargo build --locked --release --example memory-bench --bin procmail-rs
target/release/examples/memory-bench \
    target/release/procmail-rs /path/to/procmail /path/to/formail
```

GNU `/usr/bin/time` is required. No reference executable is required by normal
tests or builds. The benchmark generates private temporary messages and rc
files, invokes only the supplied reference programs, and removes its files
afterward. All final and copy destinations explicitly discard to `/dev/null`.
It performs five alternating rounds per scenario on 1 MiB and 32 MiB messages
with a folded Subject. CSV output includes each observation, maximum RSS in
KiB and elapsed wall time in seconds. Message generation and compilation are
outside the timed processes.

The Rust baseline is commit `5b6d626`, opt-level 3 with fat LTO on Linux x86_64.
The locally built patched reference tree identifies itself as
`procmail v3.23pre 2001/09/13`; these numbers should not be attributed to an
unmodified upstream 3.22 binary. Each table cell is the median of five rounds.

| Scenario | Baseline, 1 MiB | procmail-3.22, 1 MiB | Baseline, 32 MiB | procmail-3.22, 32 MiB |
| --- | ---: | ---: | ---: | ---: |
| Direct discard | 4116 KiB | 3532 KiB | 4032 KiB | 35276 KiB |
| Body-dependent rule | 5368 KiB | 3528 KiB | 37068 KiB | 35056 KiB |
| Eight header actions | 7336 KiB | 4492 KiB | 102732 KiB | 68116 KiB |
| Full `cat` filter, then eight header actions | 7288 KiB | 7188 KiB | 102168 KiB | 68112 KiB |
| Eight header actions, then four copy recipes | 7444 KiB | 4556 KiB | 102692 KiB | 68096 KiB |
| Eight header actions, then HB regex | 7480 KiB | 4556 KiB | 102660 KiB | 68100 KiB |

Except for direct discard and the full-filter scenario, a body-dependent
condition that cannot match precedes editing. This forces staged evaluation in
Rust. Each header action sets X-Benchmark independently; the reference uses an
`fhw` formail invocation, while Rust uses a native headers action. Thus these
are equivalent filtering tasks with different implementation and subprocess
costs. Full filters run a real `cat`; copy recipes are sequential deliveries,
not simultaneous copied blocks. The final HB condition inspects the new
X-Benchmark field through the whole normalized message view.

Maximum RSS measures resident pages, including touched mappings. It is not
allocated heap size, virtual address-space size, or the simultaneous total of
all subprocesses. GNU time incorporates reaped child maxima; formail can
therefore contribute to the reference peak. Concurrent copied blocks need
separate process-tree measurements before claiming an aggregate memory gain.
Fast scenarios also fall below GNU time's elapsed-time precision, so the
reported zero durations are not evidence of zero work.

The direct-discard result confirms the bounded streaming path avoids retaining
the body. The staged editing cases have a peak approximately three times the
message size: they retain the original mapping while reconstructing complete
owned versions around header edits. These observations motivate separating
edited headers from immutable body backing. They do not establish memory
behavior for arbitrary configurations, message sizes, concurrent blocks or
other operating systems.

## Shared-body comparison on 2026-10-08

The same benchmark was repeated with a saved baseline release executable and
the revised release executable. Both runs also measured the same patched
reference procmail/formail binaries. The fixtures and rc generation were
unchanged; the table gives median peak RSS for 32 MiB messages in KiB.

| Scenario | Baseline | New version | procmail-3.22 |
| --- | ---: | ---: | ---: |
| Direct discard | 3816 | 4076 | 35148 |
| Body-dependent rule | 36996 | 37248 | 35120 |
| Eight header actions | 102476 | 37228 | 68172 |
| Full `cat` filter, then eight header actions | 101988 | 69504 | 68016 |
| Eight header actions, then four copy recipes | 102644 | 37252 | 68092 |
| Eight header actions, then HB regex | 102604 | 69900 | 68152 |

For staged native header edits the peak fell by about 64%, from roughly
100 MiB to 36 MiB, and is now below the reference's roughly 67 MiB in this
scenario. The full-filter and HB cases fell by about 32%, to roughly 68 MiB.
Header edits retain the existing body owner directly, so repeated changes do
not retain a chain of old header versions or their matching caches. Copied
branches share body backing but keep separate changed headers and caches.

There is no measured reduction for streaming/body scenarios; their modest RSS
changes also include the revised binary's startup footprint. No general
throughput improvement is claimed. Full external filters still retain their
previous input until the replacement has passed validation; partial external
filters still assemble their changed and preserved parts. HB regexes require contiguous matching
bytes. The original normalized staging mapping may also remain available for
runtime includes and shell-expanded conditions. Those are remaining sources
of memory, mapping and temporary-file cost, not removed by header sharing.

## Indexed header storage

Native edits retain one byte arena with ordered raw/name ranges and a separate
list of coalesced physical free ranges. Removal frees ranges, addition reuses a
sufficient hole, and fragmentation is compacted in place before exceeding the
arena ceiling. Output order is independent of byte offsets, so prepend and
replacement do not require shifting the header bytes.

The index survives separate header actions in both header-only and staged
evaluation. Structured address/identifier lookup and native extraction use
these ranges; byte-backed input uses the same field iterator. Header versions
and their lazy serialized/normalized caches are shared through Arc. An
extraction or effective no-op retains the storage. A real edit copies the
arena and index into a private transaction, without copying caches, retaining
earlier header versions, or copying the body. All message and header limits
are checked before accepting the edited version or publishing extractions.

This does not eliminate every header allocation: changed transactions still
copy the header arena/index, and consumers requiring contiguous header bytes
prepare a serialized cache. Folded matching can additionally prepare a
normalized header. HB matching still requires one contiguous header/body
view. Malformed input with orphan continuations or missing line delimiters
occasionally needs reindexing after joining fields so the index agrees with
the validated output bytes.

### Measurement method

The existing six scenarios were repeated on the same Linux x86_64 host with
the saved baseline executable from commit 5b6d626, a saved shared-body build
with action-local storage, and the final persistent-index build. All Rust
executables use opt-level 3 and fat LTO. Each run also measures the same
patched reference procmail/formail binaries identified above.

The optional dense fixture adds 4096 X-Padding fields, approximately 208 KiB
of headers, while retaining the same total 1 MiB or 32 MiB message size.
This stays below the default header ceiling and exposes index overhead which
the original few-field fixture barely exercises. Generate it with:

```sh
target/release/examples/memory-bench \
    /path/to/procmail-rs /path/to/procmail /path/to/formail --dense-headers
```

Both fixtures use identical rc generation, five alternating rounds and the
same GNU time peak-RSS measurement. Compare saved Rust executables using this
same harness; rebuilding alone does not preserve a baseline executable.
Process-tree totals, native platform differences and general throughput remain
outside the measurement's scope.

### Peak RSS results

All cells below are medians in KiB. Baseline is the original saved Rust build;
Intermediate version shares message bodies but keeps header storage local to
each action; New version retains the header index across actions. The
procmail-3.22 columns use the patched reference tree described above, measured
alongside New version. All Rust columns use the same workload, rather than
values from a different header fixture.

Ordinary headers, 32 MiB message:

| Scenario | Baseline | Intermediate version | New version | procmail-3.22 |
| --- | ---: | ---: | ---: | ---: |
| Direct discard | 3884 | 3736 | 3808 | 35192 |
| Body-dependent rule | 37060 | 37080 | 37128 | 35200 |
| Eight header actions | 102668 | 37060 | 37136 | 68140 |
| Full cat filter, then header actions | 102100 | 69236 | 69364 | 68196 |
| Header actions, then four copies | 102636 | 37144 | 37156 | 68104 |
| Header actions, then HB regex | 102520 | 69916 | 69928 | 68136 |

Dense headers, 32 MiB message:

| Scenario | Baseline | Intermediate version | New version | procmail-3.22 |
| --- | ---: | ---: | ---: | ---: |
| Eight header actions | 103712 | 38464 | 37912 | 67996 |
| Full cat filter, then header actions | 103052 | 70772 | 70600 | 68072 |
| Header actions, then four copies | 103468 | 38380 | 38024 | 68040 |
| Header actions, then HB regex | 103668 | 71128 | 70948 | 68104 |

Native header actions, 1 MiB message:

| Headers | Baseline | Intermediate version | New version | procmail-3.22 |
| --- | ---: | ---: | ---: | ---: |
| Ordinary | 7492 | 5400 | 5272 | 4600 |
| 4096 padding fields | 9192 | 6764 | 6356 | 4592 |

The large reduction relative to the original Rust baseline remains about 64%
for staged native header edits and about 32% for full-filter/HB cases. Body
sharing, not index storage, accounts for most of it. The ordinary-header
fixture shows no substantial additional RSS reduction from persistent
indexing. Dense 32 MiB editing scenarios show roughly 0.2 to 0.5 MiB lower
median RSS than the action-local build; five-round process peaks do not
establish a general memory improvement. Dense 1 MiB copy recipes, for example,
increased from 6296 to 6380 KiB, while native edit-only recipes decreased.

Rust still has a larger startup footprint on small messages. Full filters and
HB matching remain near or slightly above the reference peak. Persistent
indexing mainly centralizes field interpretation, avoids reparsing edited
fields, reuses deleted arena ranges, and delays contiguous header views;
it does not remove the need for a private changed-header transaction or
contiguous HB matching bytes.
