// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Resolve destinations and obtain backend handles before writing messages.

use super::*;

pub(super) fn prepare_mbox(
    destination: &Destination,
    runtime: &RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<procmail_rs::delivery::mbox::LockedMbox, OperationalError> {
    let path = Path::new(destination.path());
    let settings = RuntimeSettings::new(runtime);
    let lock_timeout = settings
        .lock_timeout()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    let lock_sleep = settings
        .lock_sleep()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    let mask = settings
        .umask()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    // Keep opening and lock acquisition together so execution receives a
    // locked mailbox. No append may run between these operations, and either
    // failure must retain its typed cause before the handle is dropped.
    let mbox = MboxFile::open(path, mask).map_err(|error| {
        report_delivery_failure(
            destination,
            DeliveryFailure::from_io(DeliveryOperation::Open, &error, false),
            format!("cannot open mbox {}: {error}", path.display()),
            trace,
        )
    })?;
    mbox.lock(lock_timeout, lock_sleep).map_err(|error| {
        report_delivery_failure(
            destination,
            DeliveryFailure::from_io(DeliveryOperation::Lock, &error, false),
            format!("cannot lock mbox {}: {error}", path.display()),
            trace,
        )
    })
}

// Resolve against the runtime state at the attempt and report expansion
// failures consistently before any backend is opened. Preparation and
// execution must agree on this failure even when LASTFOLDER is deferred.
pub(super) fn resolve_destination(
    unresolved: &Destination,
    runtime: &RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<Destination, OperationalError> {
    unresolved
        .resolve_with(|name| runtime.get(name).map(str::to_owned))
        .map_err(|error| {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            OperationalError::PermanentDestination(error.to_string())
        })
}

pub(super) fn open_sinks(
    deliveries: &[PlannedDelivery],
    durability: Durability,
    runtime: &RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<Vec<Box<dyn PendingSink>>, OperationalError> {
    let mut sinks: Vec<Box<dyn PendingSink>> = Vec::with_capacity(deliveries.len());
    for delivery in deliveries {
        sinks.push(open_sink(
            delivery.destination(),
            durability,
            delivery.umask(),
            runtime,
            trace,
        )?);
    }
    Ok(sinks)
}

pub(super) fn open_sink(
    unresolved: &Destination,
    durability: Durability,
    mask: u32,
    runtime: &RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<Box<dyn PendingSink>, OperationalError> {
    record_delivery(unresolved, DeliveryStage::Preparing, trace);
    let destination = resolve_destination(unresolved, runtime, trace)?;
    match destination.kind() {
        DestinationKind::Maildir => {
            let path = Path::new(destination.path());
            let sink = MaildirSink::create(path, durability, mask).map_err(|error| {
                report_delivery_failure(
                    &destination,
                    DeliveryFailure::from_io(DeliveryOperation::Open, &error, false),
                    format!("cannot open Maildir {}: {error}", path.display()),
                    trace,
                )
            })?;
            Ok(Box::new(sink))
        }
        DestinationKind::Mbox => {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            Err(OperationalError::Internal(format!(
                "internal error: mbox destination reached streaming delivery: {}",
                destination.path()
            )))
        }
        DestinationKind::Discard => Ok(Box::new(DiscardSink::null())),
        DestinationKind::File => {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            Err(OperationalError::Internal(format!(
                "internal error: ordered destination reached streaming delivery: {}",
                destination.path()
            )))
        }
    }
}
