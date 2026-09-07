// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(kani)]
#[kani::proof]
fn stride_contract() {
    let size: usize = kani::any();
    let actual = kcpu_slot::stride(size);
    assert_eq!(actual % 64, 0);
    if size <= usize::MAX - 63 {
        assert!(actual >= size);
        assert!(actual - size < 64);
    } else {
        assert_eq!(actual, usize::MAX - 63);
    }
}

#[cfg(kani)]
mod dynamic;

#[cfg(kani)]
mod static_slots;

#[cfg(not(kani))]
mod type_contracts;

#[cfg(kani)]
mod errors;

#[cfg(all(test, not(kani)))]
mod architecture_checks;
