<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Structured header conditions

Raw header regexes operate on transport syntax. A display name, comment,
folding sequence, encoded text, or angle bracket can therefore obscure the
value a recipe intends to test. procmail-rs provides bounded `address`,
`identifier`, and `keyword` conditions which parse selected field values before
applying a byte regex.

## Address matching

The syntax is:

```text
address FIELD[,FIELD...] ?? REGEX
address %recipient ?? REGEX
address %delivered ?? REGEX
address %sender ?? REGEX
```

Supported individual fields are:
- Message originators: `From`, `Sender`, `Reply-To`
- Recipients: `To`, `Cc`
- MTA delivery headers: `Delivered-To`, `X-Original-To`, `Envelope-To`
- Resent headers: `Resent-From`, `Resent-Sender`, `Resent-To`, `Resent-Cc`

Aliases expand to groups of related fields:
- `%recipient`: expands to `To,Cc,Resent-To,Resent-Cc,Delivered-To,X-Original-To,Envelope-To`
  to match any destination address, including subaddresses (`user+tag@example.org`),
  virtual aliases, Bcc, or forwarding;
- `%delivered`: expands to `Delivered-To,X-Original-To,Envelope-To` to match only
  the delivery envelope headers added by the receiving MTA;
- `%sender`: expands to `From,Sender,Reply-To,Resent-From,Resent-Sender` to match
  any message originator.

Field and alias names are ASCII case-insensitive. Repeated names across explicit
fields and aliases, empty list items, or unsupported names are rejected.

```text
:0
* address %sender ?? .*@trusted\.example$
trusted/

:0
* address %recipient ?? ^user\+alerts@example\.org$
alerts/

:0
* address %delivered ?? ^user\+cron@example\.org$
cron/

:0
* address To,Cc ?? .*@lists\.example\.net$
example-lists/
```

Every mailbox is tested separately. Display names, nested comments, groups,
angle brackets, separators, and folding whitespace are discarded. For example,
the regex receives `Local.Part@example.org` from:

```text
From: "Example support" (ticket queue)
    <Local.Part@Example.ORG>
```

The local part retains its spelling and case. ASCII letters in the domain are
converted to lowercase. A quoted local part remains quoted:

```text
To: Group: first@example.org, "two words"@Example.NET;
```

The two regex inputs are `first@example.org` and
`"two words"@example.net`. Matching remains case-insensitive unless the recipe
has the `D` flag.

Malformed mailbox elements are ignored rather than reinterpreted as text. A
bad element does not hide a later valid mailbox when their list boundaries can
still be identified safely; unbalanced quotes, comments, or angle syntax may
make the complete field unusable. At most 4096 valid mailbox values are
presented to one condition; exceeding that ceiling fails message evaluation
explicitly.

## Identifier and thread matching

The syntax is:

```text
identifier FIELD[,FIELD...] ?? REGEX
identifier %thread ?? REGEX
```

Supported fields are `List-Id`, `Message-Id`, `In-Reply-To`, and `References`.
The alias `%thread` expands to `Message-Id,In-Reply-To,References` and matches
any message belonging to the specified discussion thread. Field names are
ASCII case-insensitive, and repeated or unsupported names are rejected.

Angle brackets, comments, and folding whitespace are stripped. Only the inner
identifier is presented to the regex:

- For `List-Id`: ASCII letters are converted to lowercase, and the value must
  be a valid dot-atom containing at least one dot (RFC 2919). The descriptive
  phrase is ignored. `List-Id` is intentionally separate from `address`;
  treating it as a mailbox would invent an at-sign and make its list hierarchy
  ambiguous.
- For `Message-Id`, `In-Reply-To`, and `References`: every message identifier
  enclosed in angle brackets `<...>` is extracted. Token spelling and case are
  preserved. Matching remains case-insensitive by default unless the recipe has
  the `D` flag.
- When multiple identifiers appear across the selected fields (such as ancestral
  IDs in `References` or parent IDs in `In-Reply-To`), each identifier is
  tested in order until a match is found. Identical tokens are deduplicated.
- At most 4096 valid identifiers are scanned per condition; exceeding that
  ceiling fails message evaluation explicitly.

For mailing lists, `identifier List-Id` matches against the list identifier,
ignoring any descriptive phrase or comments:

```text
:0
* identifier List-Id ?? ^users\.lists\.example\.org$
lists/example-users/

:0
* identifier List-Id ?? ^linux-kernel\.vger\.kernel\.org$
lists/linux-kernel/
```

For example, given:

```text
List-Id: Linux kernel development <linux-kernel.vger.kernel.org>
```

the regex receives `linux-kernel.vger.kernel.org`. A missing or malformed
identifier does not match.

For discussion threads, `%thread` tests whether any message ID in the thread
matches the target conversation:

```text
:0
* identifier %thread ?? ^root-discuss-42@example\.org$
threads/project-launch/
```

## Keyword matching

The syntax is:

```text
keyword all ?? REGEX
keyword FIELD[,FIELD...] ?? REGEX
```

Supported fields are `Keywords`, `X-Label`, and `X-Keywords`; `all` selects all
three fields. Field names are ASCII case-insensitive, and repeated, unknown, or
mixed `all` field lists are rejected.

```text
:0
* keyword all ?? ^Urgent$
urgent/

:0 D
* keyword X-Label,X-Keywords ?? ^Project:([A-Za-z0-9_-]+)$
maildir:projects/${MATCH1}/
```

Each unique valid keyword token (printable US-ASCII without spaces or commas)
is tested separately in first-seen order. Commas, whitespace, and folding are
discarded, and malformed tokens in incoming headers are ignored. At most 4096
valid keyword tokens are scanned for one condition; exceeding that ceiling
fails message evaluation explicitly.

## Captures and negation

Capturing groups populate `MATCH1`, `MATCH2`, and later numbered variables.
The procmail `\/` marker populates `MATCH`. Captures belong to the first
mailbox or identifier which satisfies the regex.

```text
:0
* address From ?? ^([^+@]+)(?:\+[^@]+)?@example\.org$
maildir:senders/${MATCH1}/
```

Leading `!` negates the overall any-value result:

```text
:0
* ! address From ?? .*@trusted\.example$
untrusted/
```

This matches when none of the valid `From` mailboxes matches the expression.
It also matches a missing field. Use a separate raw header-presence condition
when missing and malformed fields must be distinguished.

## Interaction with header editing

Structured conditions read the current header, not only the message as it
arrived. A successful earlier `headers` action is therefore visible immediately:

```text
:0
headers {
    add Cc archive@example.com
}

:0
* address Cc ?? ^archive@example\.com$
dest/archive/
```

Conversely, a removed or renamed field no longer participates in later
structured matching.

## Planning and logging

Both condition forms require only the header section and do not by themselves
retain the body. The body can continue directly to an already selected
destination when no later operation needs it.

Metadata trace records identify only the condition kind, source line, and
result. Extracted addresses and identifiers are never logged. In values detail,
the bounded regex expression may be shown, following the same policy as other
regex conditions.

The keywords do not reserve ordinary variable names. These remain variable
conditions:

```text
* address ?? ^manual-value$
* identifier ?? ^another-value$
* keyword ?? ^third-value$
```
