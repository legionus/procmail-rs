// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Write validated messages and pass backend visibility reports to accounting.

use super::*;

pub(super) fn deliver_one_sink(
    destination: &Destination,
    message: &[u8],
    durability: Durability,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OrderedStepError> {
    let mask = RuntimeSettings::new(runtime)
        .umask()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))
        .map_err(OrderedStepError::before_publication)?;
    let mut sink = open_sink(destination, durability, mask, runtime, trace)
        .map_err(OrderedStepError::before_publication)?;
    sink.write_all(message)
        .map_err(|error| {
            report_delivery_failure(
                destination,
                DeliveryFailure::from_io(DeliveryOperation::Write, &error, false),
                format!("cannot write staged delivery: {error}"),
                trace,
            )
        })
        .map_err(OrderedStepError::before_publication)?;
    let published = match sink.commit() {
        Ok(published) => published,
        Err(error) => {
            let message = format!("cannot publish Maildir delivery: {error}");
            return apply_publication(
                PublicationAttempt::failed(
                    error.published().map(PublicationResult::Delivery),
                    error.failure(),
                    0,
                    message,
                ),
                PublicationDestinations::One(destination),
                runtime,
                trace,
            )
            .map(|_| ());
        }
    };
    apply_publication(
        PublicationAttempt::published(PublicationResult::Delivery(&published)),
        PublicationDestinations::One(destination),
        runtime,
        trace,
    )
    .map(|_| ())
}

pub(super) fn deliver_file_destination(
    unresolved: &Destination,
    message: &[u8],
    output_ending: procmail_rs::config::OutputEnding,
    durability: Durability,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OrderedStepError> {
    let destination = resolve_destination(unresolved, runtime, trace)
        .map_err(OrderedStepError::before_publication)?;
    record_delivery(&destination, DeliveryStage::Preparing, trace);

    // Treat the resolved null device as a semantic discard instead of opening
    // a hostile filesystem object. The complete message has already passed
    // input validation, while avoiding device writes also keeps mbox locking,
    // rollback, and durability assumptions limited to regular files.
    if destination.kind() == DestinationKind::Discard {
        let published =
            procmail_rs::delivery::PublishedDelivery::new(PathBuf::from(destination.path()));
        return apply_publication(
            PublicationAttempt::published(PublicationResult::Delivery(&published)),
            PublicationDestinations::One(&destination),
            runtime,
            trace,
        )
        .map(|_| ());
    }
    if destination.kind() != DestinationKind::Mbox {
        return Err(OrderedStepError::before_publication(
            OperationalError::Internal(
                "internal error: file delivery resolved to another destination type".to_owned(),
            ),
        ));
    }
    let path = Path::new(destination.path());
    let locked =
        prepare_mbox(&destination, runtime, trace).map_err(OrderedStepError::before_publication)?;
    match locked.append(message, output_ending, durability) {
        Ok(published) => apply_publication(
            PublicationAttempt::published(PublicationResult::Delivery(&published)),
            PublicationDestinations::One(&destination),
            runtime,
            trace,
        )
        .map(|_| ()),
        Err(error) => {
            let message = format!("cannot deliver to mbox {}: {error}", path.display());
            let published = error
                .published()
                .then(|| procmail_rs::delivery::PublishedDelivery::new(path.to_owned()));
            apply_publication(
                PublicationAttempt::failed(
                    published.as_ref().map(PublicationResult::Delivery),
                    error.failure(),
                    0,
                    message,
                ),
                PublicationDestinations::One(&destination),
                runtime,
                trace,
            )
            .map(|_| ())
        }
    }
}
