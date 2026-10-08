// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const ROUNDS: usize = 5;
const HEADER: &[u8] =
    b"From: sender@example.org\nTo: reader@example.org\nSubject: ordinary\n folded subject\n\n";

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();

    let dense_headers = arguments.get(3).is_some_and(|arg| arg == "--dense-headers");

    if arguments.len() != 3 && !(arguments.len() == 4 && dense_headers) {
        return Err("usage: memory-bench PROCMAIL_RS PROCMAIL FORMAIL [--dense-headers]".into());
    }

    let executables: Vec<_> = arguments
        .iter()
        .take(3)
        .map(fs::canonicalize)
        .collect::<Result<_, _>>()?;
    let directory = private_directory()?;
    let result = benchmark(&directory, &executables, dense_headers);
    fs::remove_dir_all(&directory)?;
    result
}

fn private_directory() -> Result<PathBuf, Box<dyn Error>> {
    use std::os::unix::fs::DirBuilderExt;

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-memory-bench-{}-{timestamp}",
        std::process::id()
    ));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    Ok(path)
}

fn shell_path(path: &Path) -> Result<String, Box<dyn Error>> {
    let text = path.to_str().ok_or("benchmark paths must be UTF-8")?;
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

fn benchmark(
    directory: &Path,
    programs: &[PathBuf],
    dense_headers: bool,
) -> Result<(), Box<dyn Error>> {
    let formail = shell_path(&programs[2])?;
    println!("implementation,scenario,message_bytes,round,peak_rss_kib,elapsed_seconds");
    let mut header = HEADER[..HEADER.len() - 1].to_vec();

    // Ordinary fixtures have very few fields, so startup and body retention
    // dominate RSS. The optional fixture isolates index overhead while staying
    // below the same default 256 KiB header ceiling in every implementation.
    if dense_headers {
        for _ in 0..4096 {
            header.extend_from_slice(b"X-Padding: 0123456789012345678901234567890123456789\n");
        }
    }

    header.push(b'\n');

    // Generate input outside the measured processes. Bound the fixture size
    // independently of rc limits and use exclusive files in a private directory
    // so fixture construction cannot overwrite an unrelated path.
    for size in [1024usize * 1024, 32 * 1024 * 1024] {
        let message = directory.join(format!("message-{size}"));
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&message)?;
        file.write_all(&header)?;
        let block = [b'x'; 8192];
        let mut remaining = size
            .checked_sub(header.len())
            .ok_or("header exceeds fixture size")?;

        while remaining != 0 {
            let count = remaining.min(block.len());
            file.write_all(&block[..count])?;
            remaining -= count;
        }

        drop(file);

        for scenario in [
            "stream",
            "body",
            "headers",
            "filter-headers",
            "copies",
            "hb",
        ] {
            let mut rust = format!(
                "MAILDIR={}\nLIMIT_MSG_SIZE=64m\nLIMIT_MSG_BODY=64m\n",
                shell_path(directory)?
            );
            let mut original = format!("MAILDIR={}\nLOGABSTRACT=no\n", shell_path(directory)?);

            if scenario == "filter-headers" {
                rust.push_str(":0 fw\n| cat\n");
                original.push_str(":0 fw\n| cat\n");
            } else if scenario != "stream" {
                // A body-dependent no-op precedes editing in both programs.
                // It forces Rust's staged execution without altering bytes.
                let guard = ":0 B\n* impossible-body-marker\n/dev/null\n";
                rust.push_str(guard);
                original.push_str(guard);
            }

            if matches!(scenario, "headers" | "filter-headers" | "copies" | "hb") {
                for index in 0..8 {
                    rust.push_str(&format!(":0\nheaders {{\n set X-Benchmark {index}\n}}\n"));
                    original.push_str(&format!(":0 fhw\n| {formail} -I 'X-Benchmark: {index}'\n"));
                }
            }

            if scenario == "copies" {
                for _ in 0..4 {
                    rust.push_str(":0 c\n/dev/null\n");
                    original.push_str(":0 c\n/dev/null\n");
                }
            }

            if scenario == "hb" {
                let condition = ":0 HB\n* ^X-Benchmark: 7$\n/dev/null\n";
                rust.push_str(condition);
                original.push_str(condition);
            }

            rust.push_str(":0\n/dev/null\n");
            original.push_str(":0\n/dev/null\n");
            let rust_rc = directory.join("rust.rc");
            let original_rc = directory.join("original.rc");
            fs::write(&rust_rc, rust)?;
            fs::write(&original_rc, original)?;

            // Alternate implementations within each round to reduce drift.
            // GNU time reports one process peak (including child maxima),
            // not the sum of memory retained by a concurrent process tree.
            for round in 1..=ROUNDS {
                for (name, binary, config) in [
                    ("rust", &programs[0], &rust_rc),
                    ("original", &programs[1], &original_rc),
                ] {
                    let measurements = directory.join("time.txt");
                    let mut command = Command::new("/usr/bin/time");
                    command
                        .args(["-f", "%M,%e", "-o"])
                        .arg(&measurements)
                        .arg(binary);

                    if name == "rust" {
                        command.args(["filter", "--config"]).arg(config);
                    } else {
                        command.arg("-m").arg(config);
                    }

                    let output = command
                        .stdin(File::open(&message)?)
                        .stdout(Stdio::null())
                        .stderr(Stdio::piped())
                        .output()?;

                    if !output.status.success() {
                        return Err(format!(
                            "{name}/{scenario}: {}",
                            String::from_utf8_lossy(&output.stderr)
                        )
                        .into());
                    }

                    let measurement = fs::read_to_string(&measurements)?;
                    println!("{name},{scenario},{size},{round},{}", measurement.trim());
                }
            }
        }
    }

    Ok(())
}
