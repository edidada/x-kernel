// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{DirMapping, SimpleDir, SimpleFile, SimpleFs};

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    let mut net = DirMapping::new();
    net.add(
        "ip_tables_names",
        SimpleFile::new_regular(fs.clone(), || Ok("filter\n")),
    );
    root.add("net", SimpleDir::new_maker(fs, Arc::new(net)));
}
