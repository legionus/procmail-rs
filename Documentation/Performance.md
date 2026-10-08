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

| Operation | Baseline median | Updated median |
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
Header-only and body-only filters still build a bounded replacement containing
the changed area and the preserved area. Native header changes also retain the
existing complete-message representation, which copies the preserved body.

Each replacement owns an immutable version shared with copied recipe branches.
Its full normalized regex view is prepared only when a resolved HB condition
actually reaches matching, then reused by later conditions and shared branches.
A subsequent filter or header change creates a new version with an empty
cache. Headers, body-only regexes, captures, delivery, and TRAP borrow their
appropriate views without forcing that cache.

Original folded input uses a separate mapped matching file when the compiled
plan needs full matching. Raw message access for an external command does not
itself request that file. Runtime includes/switches and shell-expanded
conditions remain conservative: they can introduce HB regexes that were not
known before execution. Keeping the original normalized view available prevents
those conditions from silently matching raw folded headers instead.
