// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PMU-owned global retirement; KIRQ owns neither counter stop nor CPU calls.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::{KError, KResult};
use kirq::NmiShutdown;

struct Closing {
    claims: [Option<NmiShutdown>; 2],
    pending: [bool; kbuild_config::NR_CPUS],
    admitted: [bool; 2],
    retired: [bool; 2],
}

enum State {
    Registered,
    Closing(Closing),
    Stopped,
}

static OWNER: klazy::Lazy<kmutex::Mutex<State>> =
    klazy::Lazy::new(|| kmutex::Mutex::new(State::Registered));

// Local start checks this under its whole-transaction CPU/IRQ guard. An older
// start must finish before the same CPU can execute the synchronous stop call.
// Release/acquire publishes the gate; IPI completion, not this flag, proves stop.
// Visit every CPU allowed to start, even without a preparation record. Its stop
// callback drains any old pinned arm before restore can reopen this gate.
static IS_CLOSED: AtomicBool = AtomicBool::new(false);

pub(crate) fn can_start() -> bool {
    !IS_CLOSED.load(Ordering::Acquire)
}

fn validate_context() -> KResult<()> {
    if !kiface_task_api::TaskBlockingContextIf::is_sleepable_context() {
        return Err(KError::InvalidInput);
    }
    if !super::REGISTERED.load(Ordering::Acquire) {
        return Err(KError::Unsupported);
    }
    Ok(())
}

pub(crate) fn shutdown() -> KResult<()> {
    shutdown_with(stop_cpu)
}

fn shutdown_with(mut stop: impl FnMut(usize) -> KResult<()>) -> KResult<()> {
    validate_context()?;
    let mut state = OWNER.lock();
    if matches!(*state, State::Stopped) {
        return Ok(());
    }
    if matches!(*state, State::Registered) {
        IS_CLOSED.store(true, Ordering::Release);
        let start_cpus = if cfg!(feature = "smp") {
            kcpu_id_map::nr_cpus()
        } else {
            1
        };
        *state = State::Closing(Closing {
            claims: [None, None],
            pending: core::array::from_fn(|cpu| cpu < start_cpus),
            admitted: [false; 2],
            retired: [false; 2],
        });
    }
    let State::Closing(closing) = &mut *state else {
        return Err(KError::BadState);
    };
    for (index, source) in [super::Source::Periodic, super::Source::Notification]
        .into_iter()
        .enumerate()
    {
        if !closing.admitted[index] {
            let claim = kirq::begin_nmi_shutdown(super::descriptor(source))
                .map_err(|_| KError::ResourceBusy)?;
            for (cpu, pending) in closing.pending.iter_mut().enumerate() {
                *pending |= claim.is_participant(cpu);
            }
            closing.claims[index] = Some(claim);
            closing.admitted[index] = true;
        }
    }
    for (cpu, pending) in closing.pending.iter_mut().enumerate() {
        if *pending {
            stop(cpu)?;
            *pending = false;
        }
    }
    for (index, slot) in closing.claims.iter_mut().enumerate() {
        if closing.retired[index] {
            continue;
        }
        if let Some(claim) = slot.take() {
            if let Err(claim) = claim.finish() {
                *slot = Some(claim);
                return Err(KError::ResourceBusy);
            }
            closing.retired[index] = true;
            super::REGISTERED_SOURCES.fetch_and(!(1 << index), Ordering::Release);
        }
    }
    *state = State::Stopped;
    Ok(())
}

fn stop_cpu(cpu: usize) -> KResult<()> {
    let result = Arc::new(AtomicBool::new(false));
    let completed = result.clone();
    // OWNER retains the claim across the entire synchronous operation. The
    // callback never takes OWNER; no sleepable lock is held on the target CPU.
    kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
        completed.store(super::shutdown_local(), Ordering::Release);
    })?;
    if result.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(KError::ResourceBusy)
    }
}

pub(crate) fn restore() -> KResult<()> {
    validate_context()?;
    let mut state = OWNER.lock();
    if !matches!(*state, State::Stopped) {
        return Err(KError::BadState);
    }
    if !super::register(false) {
        return Err(KError::ResourceBusy);
    }
    *state = State::Registered;
    IS_CLOSED.store(false, Ordering::Release);
    Ok(())
}
