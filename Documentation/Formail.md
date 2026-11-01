<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# formail-rs

`formail-rs` is a mail header filter and extraction utility designed as a safe,
memory-bounded drop-in replacement for the traditional `formail` tool bundled with
procmail.

It shares its header manipulation, indexed storage, line-folding, and byte-level
parser core with `procmail-rs`, ensuring identical security guarantees, bounded
resource limits, and arbitrary byte preservation.

## Usage

```text
formail-rs [-bcfgkuzUv] [-A header] [-a header] [-i header] [-I header]
           [-u header] [-U header] [-R old new] [-x header] [-X header]
           [-p prefix]
```

When installed via `make install`, a symlink `formail` is created pointing to
`formail-rs`.

## Header Modification Options

Header matching behaves as follows:
- If the pattern ends with a colon `:`, it matches field names exactly (case-insensitive).
  For example, `-I "Subject:"` matches only `Subject:` fields.
- If the pattern does not end with a colon, it performs a prefix match on field names.
  For example, `-R "X-Old-" "X-New-"` renames any header whose name begins with `X-Old-`.
- A pattern of `From ` (or `From:`) matches the mbox postmark line.

### Appending and Adding

- `-A "Field: Value"`: Appends the specified header field to the message, even if
  a field with the same name already exists.
- `-a "Field: Value"`: Appends the specified header field only if no field with
  that name already exists. If the field is `Message-ID:` with no value (e.g.
  `-a "Message-ID:"`), `formail-rs` automatically generates a compliant, unique
  RFC 5322 `Message-ID` using high-resolution timestamps and cryptographically secure
  random bytes.

### In-Place Editing and Renaming

- `-I "Field: Value"`: Replaces existing matching header fields with the specified
  field. If only the field name is supplied (e.g. `-I "X-Spam:"`), any matching
  header fields are removed without appending anything.
- `-i "Field: Value"`: Prepends `Old-` to any existing matching header fields (preserving
  them under the renamed prefix) and appends the new field. If only the field name
  is supplied (e.g. `-i "X-Spam:"`), existing fields are renamed to `Old-` without
  appending a replacement.
- `-R "Old" "New"`: Renames existing header fields matching `Old` by substituting the
  matched prefix with `New`. Preserves original values, continuation lines, and
  line endings.

### Deduplication

- `-u "Field:"`: Keeps only the *first* occurrence of matching header fields, deleting
  subsequent duplicates.
- `-U "Field:"`: Keeps only the *last* occurrence of matching header fields, deleting
  earlier duplicates.

## Header Extraction Options

- `-x "Field:"`: Extracts and prints the values of matching header fields, omitting the
  field names.
- `-X "Field:"`: Extracts and prints the matching header fields in their entirety (both
  field names and values).
- `-k`: Retains and prints the message body after extracted headers (valid only with `-X`).

### Normalization and Formatting

- `-c`: Unfolds multi-line header fields into single lines by replacing continuation
  newlines with spaces. When extracting headers (`-x` or `-X`), each field is printed
  on a single line. When modifying headers, folded fields are transformed into single-line
  representations.
- `-z`: Normalizes whitespace: ensures exactly one space follows the colon after the
  field name, and eliminates fields consisting solely of whitespace.

## Postmark and Escaping Options

- `-f`: Forces `formail-rs` to pass headers as-is without generating a missing `From `
  mbox postmark line. By default, when standard input lacks a `From ` postmark,
  `formail-rs` generates a standard mbox postmark (`From foo@bar <date>`).
- `-b`: Disables escaping of bogus `From ` lines in the message body. By default, lines
  in the body beginning with `From ` are escaped to prevent accidental mbox corruption.
- `-p "prefix"`: Specifies the escape prefix for bogus `From ` body lines (default is `>`).

## Informational Options

- `-v`: Prints program name and version to stdout.
- `-h`, `--help`: Displays command usage and available options.

## Resource Limits and Security

Like `procmail-rs`, `formail-rs` enforces strict resource bounds to prevent denial of
service from maliciously crafted or infinite inputs:

- `#![forbid(unsafe_code)]` is enforced on the binary target.
- All input reading is bounded by `MessageLimits` (defaulting to 128 MiB message limit,
  16 MiB header limit, and 64 KiB per physical/logical header field).
- Arbitrary byte sequences in headers and body are preserved without UTF-8 loss or
  corruption.
- Stricter failure semantics: operations fail closed on allocation or limit errors.

## Unsupported Legacy Options

Certain legacy procmail 3.22 `formail` options are deliberately unsupported in this
initial release:
- Mailbox splitting (`-s`, `+skip`, `-total`)
- Automated replies (`-r`, `-t`, `-k` without `-X`)
- Duplicate Message-ID database cache (`-D`, `-n`)
- Legacy digest/article reformatting (`-d`, `-e`, `-l`, `-Y`)

If any of these unsupported options are passed, `formail-rs` immediately halts with an
informative error message describing the unsupported flag.
