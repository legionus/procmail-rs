// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn complete_service_check_reports_first_missing_executor() {
    let mut trace = NoTrace;
    let mut delivery =
        |_: &Destination,
         _: &[u8],
         _: OutputEnding,
         _: Option<&str>,
         _: &mut RuntimeVariables,
         _: &mut NoTrace| { Ok::<(), DeliveryAttemptError<()>>(()) };

    let error = match ExecutionServices::new(&mut delivery, &mut trace).require_complete() {
        Ok(_) => panic!("incomplete services unexpectedly passed validation"),
        Err(error) => error,
    };

    assert_eq!(error.missing(), "external condition");
    assert_eq!(
        error.to_string(),
        "execution service 'external condition' is unavailable"
    );
}
