// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Context-policy tests not covered by the worker lifecycle tests in
//! `worker.rs` (which observe the spawned threads directly).

use unittest::def_test;

use crate::{
    context::{AsyncContext, PreparedOp},
    worker::ensure_worker,
};

#[def_test(user, serial)]
fn shutdown_context_refuses_worker_spawn() {
    let ctx = AsyncContext::new().expect("context creation");

    ctx.push_ready(PreparedOp { cookie: 7 })
        .expect("op below queue limit");
    ctx.shutdown();
    ensure_worker(&ctx).expect("ensure_worker must not fail on a shut-down context");

    // A shut-down context must not spawn workers: there is nothing left to
    // serve, and a fresh thread would only have to observe shutdown and exit.
    assert_eq!(ctx.total_workers(), 0);
}
