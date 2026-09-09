<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Migration and rollback

`procmail-rs` deliberately does not provide a transparent fallback mailbox.
Migration therefore starts by making every successful destination and every
failure response explicit. Read `Documentation/Compatibility.md` before using
an existing procmail rc file; accepted syntax can still have documented
differences in regular expressions, trust checks, locking, and error handling.

## Prepare the configuration

1. Preserve the current procmail binary, rc files, wrapper, MTA configuration,
   mailbox locations, ownership, and modes. Record the command and exit-status
   mapping used by the MTA.
2. Work on a copy of the rc tree. Replace implicit delivery with explicit mbox,
   Maildir, or `/dev/null` destinations. Do not add a catch-all destination
   merely to hide status 79: an undelivered message must remain visible to the
   caller's queue or quarantine policy.
3. Run `procmail-rs check --config PATH`. Dynamic `INCLUDERC` and `SWITCHRC`
   paths are checked only when message processing reaches them, so exercise
   every message-selected branch separately.
4. Run `procmail-rs explain --config PATH` and inspect the value-free execution
   plan. Then replay representative copied messages into separate test
   destinations and compare results, exit statuses, and metadata-only logs.

Rc files are trusted code because pipe actions, program conditions, command
substitutions, and `TRAP` invoke the configured shell. Test using copies of
messages and isolated destinations; never feed a comparison run from the live
queue into the live mailbox twice.

## Install and switch

A staged installation does not modify the running system:

```text
make install DESTDIR=/tmp/procmail-rs-image PREFIX=/usr
```

Install that image with the site's package manager or equivalent ownership
controls. Keep the old and new executables at distinct paths. Change the MTA or
delivery wrapper in one operation so each message is handed to only one
filter. The caller must retain the message for every result except status 0;
the complete status table and a wrapper example are in `README.md`.

Use distinct test mailboxes during a gradual comparison. If original procmail
and procmail-rs must append to the same mbox during the switch, configure the
documented locking mode understood by every writer and review
`Documentation/Delivery.md`. Mixed locking schemes do not coordinate. Maildir
writers should still use separate test directories so a comparison does not
publish duplicate messages into the live `new` directory.

## Roll back

1. Stop or pause the handoff from the MTA while changing the delivery command.
   Leave unacknowledged messages in the MTA queue.
2. Restore the recorded original command, rc tree, exit-status mapping, and
   permissions as one operational change.
3. Resume delivery and inspect both filters' logs and destinations for messages
   that were already published. Do not replay a message after either filter
   returned success. A failed multi-destination run may also have completed a
   copy delivery, so blind replay can create duplicates even when the final
   status was nonzero.
4. Preserve the failed configuration, logs, and affected message identifiers
   for diagnosis. Logs alone are not message storage and may intentionally omit
   values and paths.

Do not implement rollback by invoking original procmail automatically whenever
procmail-rs fails. The first process may already have published a copy, changed
an external program's state, or run `TRAP`; an automatic second pass cannot
know which effects are safe to repeat.
