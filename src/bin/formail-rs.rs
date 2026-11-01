// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

use std::env;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use procmail_rs::delivery::mbox::Postmark;
use procmail_rs::header_edit::{EditedHeader, strip_line_ending};
use procmail_rs::hostname::current_hostname;
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;

const EX_USAGE: u8 = 64;
const EX_DATAERR: u8 = 65;
const EX_SOFTWARE: u8 = 70;
const EX_IOERR: u8 = 74;

static MSGID_COUNTER: AtomicUsize = AtomicUsize::new(1);

#[derive(Debug)]
struct Options {
    force_format: bool,
    escape_bogus: bool,
    quote_prefix: Vec<u8>,
    concatenate: bool,
    zap: bool,
    keep_body: bool,
    rename_rules: Vec<(Vec<u8>, Vec<u8>)>,
    prefix_rules: Vec<Vec<u8>>,
    delete_rules: Vec<Vec<u8>>,
    unique_first: Vec<Vec<u8>>,
    unique_last: Vec<Vec<u8>>,
    add_if_not: Vec<Vec<u8>>,
    add_always: Vec<Vec<u8>>,
    extract_values: Vec<Vec<u8>>,
    extract_fields: Vec<Vec<u8>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            force_format: false,
            escape_bogus: true,
            quote_prefix: b">".to_vec(),
            concatenate: false,
            zap: false,
            keep_body: false,
            rename_rules: Vec::new(),
            prefix_rules: Vec::new(),
            delete_rules: Vec::new(),
            unique_first: Vec::new(),
            unique_last: Vec::new(),
            add_if_not: Vec::new(),
            add_always: Vec::new(),
            extract_values: Vec::new(),
            extract_fields: Vec::new(),
        }
    }
}

fn print_version() {
    eprintln!("formail-rs 0.2.0 (procmail-rs 0.2.0)");
}

fn print_help() {
    println!(
        "Usage: formail-rs [-bczfk] [-p prefix] [-A header] [-a header] [-i header]\n       \
                [-I header] [-u header] [-U header] [-R old new]\n       \
                [-x header] [-X header] [-v] [-h]\n\n\
        Options:\n  \
          -A header        Append headerfield onto the header in any case\n  \
          -a header        Append headerfield if not present (generates Message-ID if empty)\n  \
          -i header        Rename existing matching fields to Old- and append header\n  \
          -I header        Delete existing matching fields and append header\n  \
          -u header        Make first occurrence of header unique, delete later ones\n  \
          -U header        Make last occurrence of header unique, delete earlier ones\n  \
          -R old new       Rename occurrences of old fieldname into new fieldname\n  \
          -x header        Extract contents of this headerfield\n  \
          -X header        Extract field name and contents of this headerfield\n  \
          -c               Concatenate continued fields in header (unfold)\n  \
          -z               Zap whitespace / ensure whitespace after colon\n  \
          -f               Pass along non-mailbox format (do not generate 'From ' line)\n  \
          -b               Don't escape bogus mailbox 'From ' lines\n  \
          -p prefix        Define quotation prefix (default '>')\n  \
          -k               Keep body when extracting fields\n  \
          -v               Print version and exit\n  \
          -h, -?, --help   Print this help and exit"
    );
}

fn split_field_name_and_value(field: &[u8]) -> (&[u8], Option<&[u8]>) {
    if let Some(colon_pos) = field.iter().position(|b| *b == b':') {
        let name_with_colon = &field[..colon_pos + 1];
        let val = &field[colon_pos + 1..];
        let trimmed_val = strip_line_ending(val);
        if trimmed_val.iter().any(|b| *b != b' ' && *b != b'\t') {
            (name_with_colon, Some(val))
        } else {
            (name_with_colon, None)
        }
    } else {
        (field, None)
    }
}

fn is_message_id_pattern(pattern: &[u8]) -> bool {
    let clean = pattern.strip_suffix(b":").unwrap_or(pattern);
    clean.eq_ignore_ascii_case(b"Message-ID") || clean.eq_ignore_ascii_case(b"Resent-Message-ID")
}

fn generate_message_id(pattern: &[u8]) -> Vec<u8> {
    let header_name = if pattern.to_ascii_lowercase().starts_with(b"resent-") {
        "Resent-Message-ID"
    } else {
        "Message-ID"
    };
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id();
    let counter = MSGID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hostname = current_hostname().unwrap_or_else(|_| "localhost".to_string());

    format!("{header_name}: <{time}.{pid}.{counter}@{hostname}>\n").into_bytes()
}

fn parse_args() -> Result<Option<Options>, (String, u8)> {
    let mut args = env::args_os().skip(1);
    let mut options = Options::default();

    while let Some(arg) = args.next() {
        let arg_bytes = arg.into_encoded_bytes();
        if arg_bytes == b"-h" || arg_bytes == b"-?" || arg_bytes == b"--help" {
            print_help();
            return Ok(None);
        }
        if arg_bytes == b"-v" || arg_bytes == b"--version" {
            print_version();
            return Ok(None);
        }
        if !arg_bytes.starts_with(b"-") || arg_bytes == b"-" {
            return Err((
                format!(
                    "formail-rs: unexpected argument: {}",
                    String::from_utf8_lossy(&arg_bytes)
                ),
                EX_USAGE,
            ));
        }

        let mut chars = arg_bytes[1..].iter().peekable();
        while let Some(&c) = chars.next() {
            match c {
                b'f' => options.force_format = true,
                b'b' => options.escape_bogus = false,
                b'c' => options.concatenate = true,
                b'z' => options.zap = true,
                b'k' => options.keep_body = true,
                b'h' | b'?' => {
                    print_help();
                    return Ok(None);
                }
                b'v' => {
                    print_version();
                    return Ok(None);
                }
                b'p' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -p".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    options.quote_prefix = val;
                    break;
                }
                b'x' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -x".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    options.extract_values.push(val);
                    break;
                }
                b'X' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -X".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    options.extract_fields.push(val);
                    break;
                }
                b'u' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -u".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    options.unique_first.push(val);
                    break;
                }
                b'U' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -U".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    options.unique_last.push(val);
                    break;
                }
                b'A' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -A".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    if !val.contains(&b':') {
                        return Err((
                            format!(
                                "formail-rs: invalid field '{}': missing colon",
                                String::from_utf8_lossy(&val)
                            ),
                            EX_USAGE,
                        ));
                    }
                    options.add_always.push(val);
                    break;
                }
                b'a' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -a".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    options.add_if_not.push(val);
                    break;
                }
                b'i' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -i".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    let (pattern, value) = split_field_name_and_value(&val);
                    options.prefix_rules.push(pattern.to_vec());
                    if value.is_some() {
                        options.add_always.push(val);
                    }
                    break;
                }
                b'I' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -I".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    let (pattern, value) = split_field_name_and_value(&val);
                    options.delete_rules.push(pattern.to_vec());
                    if value.is_some() {
                        options.add_always.push(val);
                    }
                    break;
                }
                b'R' => {
                    let rest: Vec<u8> = chars.by_ref().copied().collect();
                    let old_val = if !rest.is_empty() {
                        rest
                    } else {
                        args.next()
                            .ok_or_else(|| {
                                ("formail-rs: missing argument to -R".to_string(), EX_USAGE)
                            })?
                            .into_encoded_bytes()
                    };
                    let new_val = args
                        .next()
                        .ok_or_else(|| {
                            (
                                "formail-rs: missing second argument to -R".to_string(),
                                EX_USAGE,
                            )
                        })?
                        .into_encoded_bytes();
                    options.rename_rules.push((old_val, new_val));
                    break;
                }
                other => {
                    return Err((
                        format!("formail-rs: unsupported option '-{}'", other as char),
                        EX_USAGE,
                    ));
                }
            }
        }
    }

    Ok(Some(options))
}

fn stream_body(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    escape_bogus: bool,
    quote_prefix: &[u8],
) -> Result<(), (String, u8)> {
    if !escape_bogus {
        io::copy(reader, writer)
            .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
        return Ok(());
    }

    let mut at_line_start = true;
    let mut prefix: Vec<u8> = Vec::with_capacity(32);

    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| (format!("formail-rs: read error: {e}"), EX_IOERR))?;
        if available.is_empty() {
            if !prefix.is_empty() {
                writer
                    .write_all(&prefix)
                    .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
            }
            break;
        }

        let mut offset = 0;
        while offset < available.len() {
            if at_line_start {
                let byte = available[offset];
                let non_quote = prefix.iter().take_while(|b| **b == b'>').count();
                if non_quote == prefix.len() && byte == b'>' {
                    if prefix.len() < 1024 {
                        prefix.push(byte);
                        offset += 1;
                        continue;
                    } else {
                        // Excessive '>' prefix; flush and exit prefix accumulation
                        writer
                            .write_all(&prefix)
                            .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
                        prefix.clear();
                        at_line_start = false;
                    }
                } else {
                    let from_target = b"From ";
                    let matched_so_far = prefix.len() - non_quote;
                    if matched_so_far < from_target.len() && byte == from_target[matched_so_far] {
                        prefix.push(byte);
                        offset += 1;
                        if prefix.len() - non_quote == from_target.len() {
                            // Full match for "From "
                            writer
                                .write_all(quote_prefix)
                                .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
                            writer
                                .write_all(&prefix)
                                .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
                            prefix.clear();
                            at_line_start = false;
                        }
                        continue;
                    } else {
                        // Does not match "From "
                        if !prefix.is_empty() {
                            writer
                                .write_all(&prefix)
                                .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
                            prefix.clear();
                        }
                        at_line_start = false;
                    }
                }
            }

            // InLine handling: stream until the next '\n' or end of available buffer
            if let Some(pos) = available[offset..].iter().position(|b| *b == b'\n') {
                let end = offset + pos + 1;
                writer
                    .write_all(&available[offset..end])
                    .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
                offset = end;
                at_line_start = true;
            } else {
                writer
                    .write_all(&available[offset..])
                    .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
                offset = available.len();
            }
        }

        let consumed = available.len();
        reader.consume(consumed);
    }

    Ok(())
}

fn run() -> Result<(), (String, u8)> {
    let Some(options) = parse_args()? else {
        return Ok(());
    };

    let limits = MessageLimits::default();
    let mut stdin = io::stdin().lock();
    let head = Message::read_headers(&mut stdin, limits).map_err(|err| {
        (
            format!("formail-rs: failed to read message: {err}"),
            EX_DATAERR,
        )
    })?;

    let header_bytes = head.as_bytes();
    let mut edited = EditedHeader::new(header_bytes, limits).map_err(|err| {
        (
            format!("formail-rs: failed to parse headers: {err}"),
            EX_DATAERR,
        )
    })?;

    // 1. -R renames
    for (old, new) in &options.rename_rules {
        edited.rename_matching_pattern(old, new).map_err(|err| {
            (
                format!("formail-rs: header rename failed: {err}"),
                EX_DATAERR,
            )
        })?;
    }

    // 2. -i prefixes (prepend Old-)
    for pattern in &options.prefix_rules {
        edited
            .prepend_prefix_to_matching(pattern, b"Old-")
            .map_err(|err| {
                (
                    format!("formail-rs: header rename failed: {err}"),
                    EX_DATAERR,
                )
            })?;
    }

    // 3. -I deletions
    for pattern in &options.delete_rules {
        edited.remove_matching_pattern(pattern);
    }

    // 4. -u unique first
    for pattern in &options.unique_first {
        edited.make_unique_first(pattern);
    }

    // 5. -U unique last
    for pattern in &options.unique_last {
        edited.make_unique_last(pattern);
    }

    // 6. -a add if not present
    for raw in &options.add_if_not {
        let (pattern, value) = split_field_name_and_value(raw);
        if !edited.has_matching_pattern(pattern) {
            if value.is_none() && is_message_id_pattern(pattern) {
                let generated = generate_message_id(pattern);
                edited.add_raw_field(&generated).map_err(|err| {
                    (
                        format!("formail-rs: failed to add message-id: {err}"),
                        EX_DATAERR,
                    )
                })?;
            } else {
                edited.add_raw_field(raw).map_err(|err| {
                    (
                        format!("formail-rs: failed to add header: {err}"),
                        EX_DATAERR,
                    )
                })?;
            }
        }
    }

    // 7. -A add always
    for raw in &options.add_always {
        edited.add_raw_field(raw).map_err(|err| {
            (
                format!("formail-rs: failed to add header: {err}"),
                EX_DATAERR,
            )
        })?;
    }

    // 8. -c concatenate continued
    if options.concatenate {
        edited.concatenate_continued_fields().map_err(|err| {
            (
                format!("formail-rs: failed to concatenate headers: {err}"),
                EX_DATAERR,
            )
        })?;
    }

    // 9. -z zap whitespace
    if options.zap {
        edited.zap_whitespace().map_err(|err| {
            (
                format!("formail-rs: failed to zap whitespace: {err}"),
                EX_DATAERR,
            )
        })?;
    }

    edited.finish(0).map_err(|err| {
        (
            format!("formail-rs: invalid header after modification: {err}"),
            EX_DATAERR,
        )
    })?;

    let mut stdout = io::stdout().lock();

    let is_extracting = !options.extract_values.is_empty() || !options.extract_fields.is_empty();
    if is_extracting {
        for pattern in &options.extract_values {
            let extracted = edited.extract_fields(pattern, false, options.concatenate, options.zap);
            for field in extracted {
                stdout
                    .write_all(&field)
                    .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
            }
        }
        for pattern in &options.extract_fields {
            let extracted = edited.extract_fields(pattern, true, options.concatenate, options.zap);
            for field in extracted {
                stdout
                    .write_all(&field)
                    .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
            }
        }
        if options.keep_body {
            stdout
                .write_all(b"\n")
                .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
            stream_body(
                &mut stdin,
                &mut stdout,
                options.escape_bogus,
                &options.quote_prefix,
            )?;
        }
    } else {
        let has_from = edited.field_count() > 0 && edited.field(0).bytes.starts_with(b"From ");
        if !has_from && !options.force_format {
            let postmark = Postmark::generated(SystemTime::now()).map_err(|e| {
                (
                    format!("formail-rs: failed to generate postmark: {e:?}"),
                    EX_SOFTWARE,
                )
            })?;
            stdout
                .write_all(postmark.as_bytes())
                .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
        }

        stdout
            .write_all(edited.as_bytes())
            .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;

        if !edited.as_bytes().ends_with(b"\n\n") && !edited.as_bytes().ends_with(b"\r\n\r\n") {
            stdout
                .write_all(b"\n")
                .map_err(|e| (format!("formail-rs: write error: {e}"), EX_IOERR))?;
        }

        stream_body(
            &mut stdin,
            &mut stdout,
            options.escape_bogus,
            &options.quote_prefix,
        )?;
    }

    stdout
        .flush()
        .map_err(|e| (format!("formail-rs: flush error: {e}"), EX_IOERR))?;
    Ok(())
}

fn main() -> ExitCode {
    if let Err((msg, code)) = run() {
        eprintln!("{msg}");
        ExitCode::from(code)
    } else {
        ExitCode::SUCCESS
    }
}
