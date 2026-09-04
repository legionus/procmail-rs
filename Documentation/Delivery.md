<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Delivery, paths, locking, and durability

## Destination syntax and path resolution

`maildir:PATH` and a recipe path ending in `/` select Maildir. `mbox:PATH` and
an unmarked path select mboxrd. This choice comes only from syntax; the program
never inspects the current filesystem type to choose a backend.

For compatibility with documented procmail discard recipes, an unmarked path
which resolves exactly to `/dev/null` is an explicit discard destination. The
message is still read and checked against every active input limit, but the
program does not open or write the device. Explicit `mbox:/dev/null`, paths to
other devices, and lookalike paths such as `/dev/null/` are not discard
destinations and continue through their selected backend's normal checks.

Absolute paths remain absolute. A relative path is resolved against `MAILDIR`
active when that statement executes, or against the process working directory
when `MAILDIR` is unset. Destination expressions remain bounded until their
recipe is selected, so values such as `$MATCH` and `$LASTFOLDER` can participate
in runtime selection. Empty paths, NUL, `.` or `..` components, repeated
separators, and an unexpected trailing separator are rejected.

All Maildir components, including `tmp`, `new`, and `cur`, are opened relative
to an already opened directory with symlink following disabled. The three
directories must already exist. Mbox parent components and the final regular
file receive equivalent checks; a final mbox symlink or a file with more than
one hard link is rejected.

## Durability

`DURABILITY` accepts `none` (the default), `file`, or `full`:

| Setting | Maildir | mbox |
| --- | --- | --- |
| `none` | Atomic publication from an unnamed `O_TMPFILE`, without `fsync` | Complete append while locked, without `fsync` |
| `file` | `fsync` message before publication | `fsync` mailbox after append |
| `full` | As `file`, then `fsync` both `tmp` and `new` after publication | As `file`, then `fsync` the parent directory |

Maildir publication uses a no-replace, descriptor-relative rename from `tmp`
to `new`. A `full` directory-sync failure occurs after publication and is
reported as such; retrying can create a duplicate. The implementation makes no
claim about filesystems that do not provide the Linux operations or persistence
semantics used here.

Mbox appends are serialized with `flock`, using `LOCKTIMEOUT`. The original
length is recorded while locked. A failed append or sync attempts to truncate
back to that length before unlocking; failed rollback is an internal error.
Normal delivery appends only enough LF bytes for the record to end with one
empty line; an existing empty line is not extended. Raw `r` delivery adds only
the LF needed to keep a following postmark on a new line.
Concurrent tests cover cooperating writers on a local filesystem. No NFS
safety claim is made, and non-cooperating writers can still corrupt a mailbox.

Local recipe and global locks default to `LOCKMETHOD=flock` and use a secure,
persistent lock file. `LOCKMETHOD=dotlock` provides compatibility with the
original named-file scheme, including its pathname replacement risk and stale
lock removal. See [Compatibility.md](Compatibility.md) for the exact behavioral
differences.
