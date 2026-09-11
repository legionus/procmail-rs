<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Native header editing

The `headers { ... }` action handles common header-only transformations without
starting `formail` or copying the message through a child process. A selected
block applies every operation in source order and then continues with the next
recipe.

```text
:0
headers {
    remove X-Old-Status
    set X-Spam-Status clean
    add X-Filter-Result passed
    prepend X-Processed-By procmail-rs
}
```

## Operations

`remove NAME` removes every field with that ASCII case-insensitive name,
including its continuation lines.

```text
headers {
    remove X-Spam-Flag
    remove X-Virus-Status
}
```

`set NAME [VALUE]` replaces the first matching field in place, removes later
duplicates, and appends a field when none exists. An omitted value creates an
empty field.

```text
headers {
    set Subject Filtered message
    set X-Reviewed
}
```

`add NAME [VALUE]` appends a field even when the name already exists. This is
appropriate for list-valued trace fields such as `Received`, but callers remain
responsible for whether a field may legally be repeated.

```text
headers {
    add X-Filter-Step antivirus
    add X-Filter-Step spam-check
}
```

`prepend NAME [VALUE]` inserts a field before every existing field. A later
prepend operation therefore appears before an earlier one.

```text
headers {
    prepend X-Processing-Hop first
    prepend X-Processing-Hop second
}
```

`rename OLD-NAME to NEW-NAME` changes every matching name while preserving its
value, folding, order, and line endings.

```text
headers {
    rename X-Old-Category to X-Category
}
```

Extraction assigns the first matching field to an ordinary user variable:

```text
headers {
    extract raw X-Original-Value into RAW_VALUE
    extract unfolded Subject into SUBJECT_TEXT
    extract decoded Subject into DECODED_SUBJECT
}
```

- `raw` removes the field name, colon, and final line ending but retains leading
  whitespace and folding.
- `unfolded` removes leading horizontal whitespace from each physical line and
  joins continuation lines with one space.
- `decoded` additionally decodes strict RFC 2047 `B` and `Q` encoded-words in
  UTF-8, US-ASCII, or ISO-8859-1. Other charsets and malformed encoded-words
  fail the complete action.

An absent field assigns an empty value. Repeated extraction into the same
variable leaves the result of the last operation.

## Values and field names

Field names follow RFC 5322 `1*ftext`: printable ASCII bytes from 33 through
126 except colon. Whitespace and non-ASCII names are rejected. Matching ignores
ASCII case; newly generated names use stable title case at hyphen boundaries.

Values use the bounded syntax from [ShellExpressions.md](ShellExpressions.md):

```text
CATEGORY=reports

:0
headers {
    set X-Category ${CATEGORY:-unknown}
    add X-Source "filter-$PROCMAIL_VERSION"
}
```

Printable US-ASCII, space, and tab are emitted directly. A non-ASCII Unicode
value is encoded automatically as RFC 2047 UTF-8/Base64 encoded-words. Control
bytes and requested folded continuations are rejected. Automatic encoding is a
convenience, not a declaration that RFC 2047 is valid for every possible field.

```text
headers {
    set Subject Přijatá zpráva
}
```

## Ordering and later recipes

Every operation observes changes made earlier in the same block. Later recipes
see the completed edited header, including through `address`, `identifier`, raw
header regexes, extraction, external filters, and final delivery.

```text
:0
headers {
    add Cc bar@example.com
}

:0
* address Cc ?? ^bar@example\.com$
dest/baz/
```

If expansion, encoding, validation, or a size check fails, none of the block's
changes become visible and the preceding message remains selected.

## Recipe flags

Condition and control flags `H`, `B`, `D`, `c`, `A`, `a`, `E`, and `e` retain
their usual meanings. The block always continues after success, so `c` has no
additional effect. Flags `h`, `b`, `f`, `w`, `W`, `i`, and `r`, and local
lockfiles, are rejected because this action neither starts a child nor publishes
a destination.

```text
:0 E
* ^X-Trusted: yes$
headers {
    set X-Trust-Result failed
}
```

## Resource and privacy behavior

The complete result is checked against `LIMIT_MSG_SIZE`, `LIMIT_MSG_HEADERS`,
`LIMIT_HEADER_LINE`, and `LIMIT_HEADER_FIELD`. One block permits at most 256
operations. Each extracted value is bounded to 64 KiB, and values remain private
until every operation succeeds.

Trace records identify operation kinds, source lines, field names, rename
targets, and extraction variables. They never contain header values. Extracted
values remain hidden even when `LOGDETAIL=values` is enabled.

Native editing is not a complete `formail` replacement. It does not split
digests, generate message identifiers, rewrite bodies, or implement arbitrary
`formail` options. Use a trusted filter action when a transformation is outside
the operations above.

