// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use regex::bytes::RegexBuilder;

const ROUNDS: usize = 5;

fn main() {
    println!("benchmark,round,iterations,elapsed_ns,ns_per_iteration");
    bench_compile();
    bench_header_match();
    bench_body_miss();
}

fn bench_compile() {
    const ITERATIONS: usize = 5_000;
    for round in 1..=ROUNDS {
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let regex = RegexBuilder::new(black_box(
                r"^(From|To|Cc|Subject|List-Id):[ \t]*.*example\.(org|com)$",
            ))
            .case_insensitive(true)
            .multi_line(true)
            .unicode(false)
            .build()
            .unwrap();
            black_box(regex);
        }
        report("compile", round, ITERATIONS, start.elapsed());
    }
}

fn bench_header_match() {
    const ITERATIONS: usize = 5_000;
    let mut header = Vec::with_capacity(64 * 1024);
    while header.len() < 63 * 1024 {
        header.extend_from_slice(b"X-Received: by relay.example.net with SMTP id 123456789\n");
    }
    header.extend_from_slice(b"List-Id: users.example.org\n\n");
    let regex = RegexBuilder::new(r"^List-Id:[ \t]*.*example\.org$")
        .case_insensitive(true)
        .multi_line(true)
        .unicode(false)
        .build()
        .unwrap();

    for round in 1..=ROUNDS {
        let start = Instant::now();
        let mut matches = 0;
        for _ in 0..ITERATIONS {
            matches += usize::from(regex.is_match(black_box(&header)));
        }
        black_box(matches);
        report("header_match_64k", round, ITERATIONS, start.elapsed());
    }
}

fn bench_body_miss() {
    const ITERATIONS: usize = 200;
    let mut body = Vec::with_capacity(1024 * 1024);
    while body.len() < 1024 * 1024 {
        body.extend_from_slice(
            b"ordinary message text without the header being searched for anywhere\n",
        );
    }
    let regex = RegexBuilder::new(r"^X-Spam-Flag:[ \t]*YES$")
        .case_insensitive(true)
        .multi_line(true)
        .unicode(false)
        .build()
        .unwrap();

    for round in 1..=ROUNDS {
        let start = Instant::now();
        let mut matches = 0;
        for _ in 0..ITERATIONS {
            matches += usize::from(regex.is_match(black_box(&body)));
        }
        black_box(matches);
        report("body_miss_1m", round, ITERATIONS, start.elapsed());
    }
}

fn report(name: &str, round: usize, iterations: usize, elapsed: Duration) {
    println!(
        "{name},{round},{iterations},{},{}",
        elapsed.as_nanos(),
        elapsed.as_nanos() / iterations as u128
    );
}
