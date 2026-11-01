// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;
use crate::header_edit::HeaderView;

#[test]
fn parses_content_type_media_types_and_parameters() {
    let simple = parse_content_type(b"text/plain");
    assert_eq!(simple.mimetype.as_deref(), Some(b"text/plain".as_slice()));
    assert_eq!(simple.boundary, None);
    assert_eq!(simple.name, None);

    let upper = parse_content_type(b"TEXT/HTML; Charset=\"UTF-8\"");
    assert_eq!(upper.mimetype.as_deref(), Some(b"text/html".as_slice()));

    let multipart = parse_content_type(b"multipart/mixed; boundary=\"----=_Part_123_456\"");
    assert_eq!(
        multipart.mimetype.as_deref(),
        Some(b"multipart/mixed".as_slice())
    );
    assert_eq!(
        multipart.boundary.as_deref(),
        Some(b"----=_Part_123_456".as_slice())
    );

    let unquoted_bound = parse_content_type(b"multipart/alternative; boundary=simple-boundary-42");
    assert_eq!(
        unquoted_bound.boundary.as_deref(),
        Some(b"simple-boundary-42".as_slice())
    );

    let with_name = parse_content_type(b"application/pdf; name=\"annual_report.pdf\"");
    assert_eq!(
        with_name.mimetype.as_deref(),
        Some(b"application/pdf".as_slice())
    );
    assert_eq!(
        with_name.name.as_deref(),
        Some(b"annual_report.pdf".as_slice())
    );

    let with_comment = parse_content_type(b"text/plain (plain text format); charset=us-ascii");
    assert_eq!(
        with_comment.mimetype.as_deref(),
        Some(b"text/plain".as_slice())
    );

    assert_eq!(parse_content_type(b"invalid-no-slash").mimetype, None);
    assert_eq!(parse_content_type(b"text/").mimetype, None);
    assert_eq!(parse_content_type(b"/plain").mimetype, None);
    assert_eq!(parse_content_type(b"text / plain").mimetype, None);
}

#[test]
fn parses_content_disposition_and_filenames() {
    let inline = parse_content_disposition(b"inline");
    assert_eq!(inline.disposition.as_deref(), Some(b"inline".as_slice()));
    assert_eq!(inline.filename, None);

    let attachment = parse_content_disposition(b"attachment; filename=\"data.csv\"");
    assert_eq!(
        attachment.disposition.as_deref(),
        Some(b"attachment".as_slice())
    );
    assert_eq!(attachment.filename.as_deref(), Some(b"data.csv".as_slice()));

    let unquoted = parse_content_disposition(b"ATTACHMENT; filename=test.patch");
    assert_eq!(
        unquoted.disposition.as_deref(),
        Some(b"attachment".as_slice())
    );
    assert_eq!(unquoted.filename.as_deref(), Some(b"test.patch".as_slice()));

    let escaped =
        parse_content_disposition(b"attachment; filename=\"file \\\"with quotes\\\".pdf\"");
    assert_eq!(
        escaped.filename.as_deref(),
        Some(b"file \"with quotes\".pdf".as_slice())
    );

    let rfc2231 =
        parse_content_disposition(b"attachment; filename*=UTF-8''my%20document%20name.pdf");
    assert_eq!(
        rfc2231.filename.as_deref(),
        Some(b"my document name.pdf".as_slice())
    );

    let rfc2047 =
        parse_content_disposition(b"attachment; filename=\"=?UTF-8?Q?invoice_2026?=.pdf\"");
    assert_eq!(
        rfc2047.filename.as_deref(),
        Some(b"invoice 2026.pdf".as_slice())
    );
}

#[test]
fn part_from_headers_combines_type_disposition_and_filename() {
    let headers = b"Content-Type: text/x-patch; name=\"fallback.diff\"\r\n\
Content-Disposition: attachment; filename=\"primary.patch\"\r\n\r\n";
    let (part, boundary) = part_from_headers(HeaderView::Raw(headers));
    assert_eq!(part.mimetype.as_deref(), Some(b"text/x-patch".as_slice()));
    assert_eq!(part.disposition.as_deref(), Some(b"attachment".as_slice()));
    assert_eq!(part.filename.as_deref(), Some(b"primary.patch".as_slice()));
    assert_eq!(boundary, None);

    // Fallback to name parameter when filename is absent:
    let headers_fallback = b"Content-Type: application/pdf; name=\"document.pdf\"\r\n\
Content-Disposition: attachment\r\n\r\n";
    let (part_fallback, _) = part_from_headers(HeaderView::Raw(headers_fallback));
    assert_eq!(
        part_fallback.filename.as_deref(),
        Some(b"document.pdf".as_slice())
    );
}

#[test]
fn single_part_message_is_visited_as_root_part() {
    let header = b"Content-Type: text/calendar; method=REQUEST\r\n\
Content-Disposition: inline\r\n\r\n";
    let body = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";

    let mut visited = Vec::new();
    let result = any_mime_part(HeaderView::Raw(header), body, |part| {
        visited.push(part.clone());
        Ok::<bool, ()>(false)
    });

    assert!(!result.unwrap());
    assert_eq!(visited.len(), 1);
    assert_eq!(
        visited[0].mimetype.as_deref(),
        Some(b"text/calendar".as_slice())
    );
    assert_eq!(
        visited[0].disposition.as_deref(),
        Some(b"inline".as_slice())
    );
}

#[test]
fn multipart_message_visits_root_and_subparts_with_preamble_ignored() {
    let header = b"Content-Type: multipart/mixed; boundary=\"bound-1\"\r\n\r\n";
    let body = b"Preamble line to ignore\r\n\
--bound-1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Disposition: inline\r\n\
\r\n\
Body text line 1\r\n\
--bound-1\r\n\
Content-Type: text/x-patch\r\n\
Content-Disposition: attachment; filename=\"0001-fix.patch\"\r\n\
\r\n\
diff --git a/a b/b\r\n\
--bound-1--\r\n\
Epilogue line to ignore\r\n";

    let mut visited = Vec::new();
    let result = any_mime_part(HeaderView::Raw(header), body, |part| {
        visited.push(part.clone());
        Ok::<bool, ()>(false)
    });

    assert!(!result.unwrap());
    assert_eq!(visited.len(), 3);

    // Root part:
    assert_eq!(
        visited[0].mimetype.as_deref(),
        Some(b"multipart/mixed".as_slice())
    );
    assert_eq!(visited[0].filename, None);

    // Part 1:
    assert_eq!(
        visited[1].mimetype.as_deref(),
        Some(b"text/plain".as_slice())
    );
    assert_eq!(
        visited[1].disposition.as_deref(),
        Some(b"inline".as_slice())
    );
    assert_eq!(visited[1].filename, None);

    // Part 2:
    assert_eq!(
        visited[2].mimetype.as_deref(),
        Some(b"text/x-patch".as_slice())
    );
    assert_eq!(
        visited[2].disposition.as_deref(),
        Some(b"attachment".as_slice())
    );
    assert_eq!(
        visited[2].filename.as_deref(),
        Some(b"0001-fix.patch".as_slice())
    );
}

#[test]
fn nested_multipart_recursively_visits_subparts() {
    let header = b"Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n";
    let body = b"--outer\r\n\
Content-Type: multipart/alternative; boundary=\"inner\"\r\n\
\r\n\
--inner\r\n\
Content-Type: text/plain\r\n\
\r\n\
Plain text\r\n\
--inner\r\n\
Content-Type: text/html\r\n\
\r\n\
<b>HTML text</b>\r\n\
--inner--\r\n\
--outer\r\n\
Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
\r\n\
%PDF-data\r\n\
--outer--\r\n";

    let mut types = Vec::new();
    any_mime_part(HeaderView::Raw(header), body, |part| {
        if let Some(mt) = &part.mimetype {
            types.push(mt.clone());
        }
        Ok::<bool, ()>(false)
    })
    .unwrap();

    assert_eq!(
        types,
        [
            b"multipart/mixed".to_vec(),
            b"multipart/alternative".to_vec(),
            b"text/plain".to_vec(),
            b"text/html".to_vec(),
            b"application/pdf".to_vec(),
        ]
    );
}

#[test]
fn short_circuits_on_visitor_match() {
    let header = b"Content-Type: multipart/mixed; boundary=\"bnd\"\r\n\r\n";
    let body = b"--bnd\r\n\
Content-Type: text/plain\r\n\
\r\n\
First part\r\n\
--bnd\r\n\
Content-Type: text/calendar\r\n\
\r\n\
Calendar\r\n\
--bnd\r\n\
Content-Type: text/x-patch\r\n\
\r\n\
Patch\r\n\
--bnd--\r\n";

    let mut count = 0;
    let matched = any_mime_part(HeaderView::Raw(header), body, |part| {
        count += 1;
        if part.mimetype.as_deref() == Some(b"text/calendar") {
            Ok::<bool, ()>(true)
        } else {
            Ok::<bool, ()>(false)
        }
    })
    .unwrap();

    assert!(matched);
    // Root + Part 1 (text/plain) + Part 2 (text/calendar) = 3 visits before short circuit
    assert_eq!(count, 3);
}

#[test]
fn ignores_boundary_not_at_start_of_line() {
    let header = b"Content-Type: multipart/mixed; boundary=\"bnd\"\r\n\r\n";
    let body = b"--bnd\r\n\
Content-Type: text/plain\r\n\
\r\n\
Line with embedded --bnd in the middle of text\r\n\
--bnd\r\n\
Content-Type: text/x-diff\r\n\
\r\n\
diff\r\n\
--bnd--\r\n";

    let mut types = Vec::new();
    any_mime_part(HeaderView::Raw(header), body, |part| {
        if let Some(mt) = &part.mimetype {
            types.push(mt.clone());
        }
        Ok::<bool, ()>(false)
    })
    .unwrap();

    assert_eq!(
        types,
        [
            b"multipart/mixed".to_vec(),
            b"text/plain".to_vec(),
            b"text/x-diff".to_vec(),
        ]
    );
}

#[test]
fn bounds_multipart_recursion_depth() {
    // Generate deeply nested multiparts beyond MAX_MIME_DEPTH
    let mut body = Vec::new();
    for d in 0..MAX_MIME_DEPTH + 2 {
        body.extend_from_slice(
            format!(
                "--bnd-{d}\r\nContent-Type: multipart/mixed; boundary=\"bnd-{}\"\r\n\r\n",
                d + 1
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(b"--bnd-deep\r\nContent-Type: text/deep\r\n\r\nDeep\r\n");

    let header = b"Content-Type: multipart/mixed; boundary=\"bnd-0\"\r\n\r\n";

    let mut types = Vec::new();
    let _ = any_mime_part(HeaderView::Raw(header), &body, |part| {
        if let Some(mt) = &part.mimetype {
            types.push(mt.clone());
        }
        Ok::<bool, ()>(false)
    });

    // Should stop recursing when depth > MAX_MIME_DEPTH
    assert!(!types.iter().any(|t| t == b"text/deep"));
}
