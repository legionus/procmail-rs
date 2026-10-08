// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Apply publication reports to tracing, LASTFOLDER, and completion state.

use super::*;

#[derive(Default)]
pub(super) struct PublicationTracker {
    published: usize,
    original_delivered: bool,
}

impl PublicationTracker {
    pub(super) fn record(
        &mut self,
        published: usize,
        original_delivered: bool,
    ) -> Result<(), OperationalError> {
        self.published = self.published.checked_add(published).ok_or_else(|| {
            OperationalError::Internal("published destination count overflows".to_owned())
        })?;
        self.original_delivered |= original_delivered;
        Ok(())
    }

    pub(super) fn record_outcome(
        &mut self,
        outcome: procmail_rs::eval::DeliveryOutcome,
    ) -> Result<(), OperationalError> {
        self.record(outcome.published(), outcome.original_delivered())
    }

    pub(super) fn finish(&mut self) -> Result<(), OperationalError> {
        // Consume the state even when the original was not delivered. This
        // keeps accidental reuse of DeliveryRuntime from carrying publication
        // counts into another message while retaining the copy count in the
        // diagnostic produced for this one.
        let completed = std::mem::take(self);
        if completed.original_delivered {
            Ok(())
        } else {
            Err(OperationalError::Undelivered(format!(
                "original message was not delivered (published {} copy destination(s))",
                completed.published
            )))
        }
    }
}

pub(super) struct OrderedStepError {
    pub(super) error: OperationalError,
    pub(super) can_handle: bool,
}

pub(super) enum PublicationDestinations<'a> {
    One(&'a Destination),
    Plan(&'a [PlannedDelivery]),
}

impl PublicationDestinations<'_> {
    fn get(&self, index: usize) -> Option<&Destination> {
        match self {
            Self::One(destination) => (index == 0).then_some(*destination),
            Self::Plan(deliveries) => deliveries.get(index).map(PlannedDelivery::destination),
        }
    }
}

struct PublicationFailure {
    failure: DeliveryFailure,
    failed_index: usize,
    error: OperationalError,
}

pub(super) struct PublicationAttempt<'a> {
    published: Option<PublicationResult<'a>>,
    failure: Option<PublicationFailure>,
}

impl<'a> PublicationAttempt<'a> {
    pub(super) fn published(result: PublicationResult<'a>) -> Self {
        Self {
            published: Some(result),
            failure: None,
        }
    }

    pub(super) fn failed(
        published: Option<PublicationResult<'a>>,
        failure: DeliveryFailure,
        failed_index: usize,
        message: String,
    ) -> Self {
        Self {
            published,
            failure: Some(PublicationFailure {
                failure,
                failed_index,
                error: OperationalError::delivery_failure(failure, message),
            }),
        }
    }
}

pub(super) fn apply_publication(
    attempt: PublicationAttempt<'_>,
    destinations: PublicationDestinations<'_>,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<usize, OrderedStepError> {
    let published = attempt.published.map_or(0, PublicationResult::len);
    // Bind the failed path before LASTFOLDER changes to a newly published name.
    // A sync failure belongs to the sink just made visible, not the next sink.
    let failed_destination = attempt.failure.as_ref().map(|failure| {
        destinations
            .get(failure.failed_index)
            .ok_or_else(|| {
                OperationalError::Internal("failed sink has no delivery plan entry".to_owned())
            })
            .and_then(|destination| {
                destination
                    .resolve_with(|name| runtime.get(name).map(str::to_owned))
                    .map_err(|error| OperationalError::Internal(error.to_string()))
            })
    });

    // Only the backend can tell whether a destination became visible. Apply
    // every externally observable consequence from that report so trace,
    // LASTFOLDER, and the returned count cannot disagree after a partial
    // fanout or a durability failure following publication.
    for index in 0..published {
        let destination = destinations.get(index).ok_or_else(|| {
            OrderedStepError::after_publication(OperationalError::Internal(
                "published destination has no matching delivery plan entry".to_owned(),
            ))
        })?;
        let resolved = destination
            .resolve_with(|name| runtime.get(name).map(str::to_owned))
            .unwrap_or_else(|_| destination.clone());
        record_delivery(&resolved, DeliveryStage::Published, trace);
    }
    if let Some(result) = attempt.published {
        runtime
            .record_publication(result, trace)
            .map_err(OperationalError::Internal)
            .map_err(OrderedStepError::after_publication)?;
    }

    if let Some(failure) = attempt.failure {
        let destination = failed_destination
            .ok_or_else(|| {
                OrderedStepError::after_publication(OperationalError::Internal(
                    "failed sink has no bound destination".to_owned(),
                ))
            })?
            .map_err(OrderedStepError::after_publication)?;
        record_delivery(&destination, DeliveryStage::Failure(failure.failure), trace);
        return Err(if published == 0 {
            OrderedStepError::before_publication(failure.error)
        } else {
            OrderedStepError::after_publication(failure.error)
        });
    }
    Ok(published)
}

impl OrderedStepError {
    pub(super) fn before_publication(error: OperationalError) -> Self {
        Self {
            error,
            can_handle: true,
        }
    }

    pub(super) fn after_publication(error: OperationalError) -> Self {
        Self {
            error,
            can_handle: false,
        }
    }
}

pub(super) fn commit_delivery(
    validated: procmail_rs::delivery::ValidatedFanout,
    deliveries: &[PlannedDelivery],
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<usize, OperationalError> {
    check_signal()?;
    let result = match validated.commit() {
        Ok(report) => apply_publication(
            PublicationAttempt::published(PublicationResult::Fanout(&report)),
            PublicationDestinations::Plan(deliveries),
            runtime,
            trace,
        ),
        Err(error) => {
            let message = format!("cannot publish Maildir delivery: {error}");
            apply_publication(
                PublicationAttempt::failed(
                    (!error.published().is_empty())
                        .then_some(PublicationResult::PartialFanout(&error)),
                    error.failure(),
                    error.failed_index(),
                    message,
                ),
                PublicationDestinations::Plan(deliveries),
                runtime,
                trace,
            )
        }
    };
    result.map_err(|error| error.error)
}
