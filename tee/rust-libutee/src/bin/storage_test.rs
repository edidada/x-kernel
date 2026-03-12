use rust_utee::{
    api::tee_api_objects::{
        TEE_AllocatePersistentObjectEnumerator, TEE_AllocateTransientObject,
        TEE_CloseAndDeletePersistentObject, TEE_CloseObject, TEE_CreatePersistentObject,
        TEE_FreePersistentObjectEnumerator, TEE_GenerateKey, TEE_GetNextPersistentObject,
        TEE_GetObjectBufferAttribute, TEE_GetObjectInfo1, TEE_OpenPersistentObject,
        TEE_ReadObjectData, TEE_ResetPersistentObjectEnumerator, TEE_RestrictObjectUsage1,
        TEE_SeekObjectData, TEE_StartPersistentObjectEnumerator, TEE_TruncateObjectData,
        TEE_USAGE_DEFAULT, TEE_WriteObjectData,
    },
    tee_api_defines::{
        TEE_ATTR_ECC_PRIVATE_VALUE, TEE_ATTR_ECC_PUBLIC_VALUE_X, TEE_ATTR_ECC_PUBLIC_VALUE_Y,
        TEE_DATA_FLAG_ACCESS_READ, TEE_DATA_FLAG_ACCESS_WRITE, TEE_DATA_FLAG_ACCESS_WRITE_META,
        TEE_DATA_FLAG_OVERWRITE, TEE_DATA_FLAG_SHARE_READ, TEE_DATA_FLAG_SHARE_WRITE,
        TEE_ERROR_ITEM_NOT_FOUND, TEE_STORAGE_PRIVATE, TEE_SUCCESS, TEE_TYPE_SM2_DSA_KEYPAIR,
        TEE_USAGE_EXTRACTABLE,
    },
    tee_api_types::{TEE_ObjectEnumHandle, TEE_ObjectHandle, TEE_ObjectInfo, TEE_Whence},
};

use rand::Rng;

type TestResult = Result<(), ()>;

fn main() {
    let funcs: Vec<fn() -> TestResult> = vec![
        test_create_raw_object,
        test_obj_basic_operations,
        test_create_object_with_initial_data,
        test_create_object_without_overwrite,
        test_create_object_with_overwrite,
        test_open_nonexistent_object,
        test_open_object_different_flags,
        test_write_data_append,
        test_write_data_large,
        test_read_data_partial,
        test_read_data_beyond_end,
        test_seek_set,
        test_seek_cur,
        test_seek_end,
        test_seek_boundary,
        test_truncate_to_zero,
        test_truncate_to_middle,
        test_truncate_to_end,
        test_delete_object,
        test_get_sm2_dsa_keypair_attributes,
        test_create_open_del_object,
        test_generate_persistant_sm2_dsa_keypair,
        test_create_two_objects,
        test_enum_single_object,
        test_create_some_objects,
        test_create_open_read_some_objects,
        test_enum_objects,
    ];

    for func in &funcs {
        let res = func();
        if res.is_ok() {
            println!("<<< test success");
        } else {
            eprintln!("<<< test failed");
        }
    }

    let mut rng = rand::rng();
    for i in 0..1000 {
        println!(">>>>>> storage_test with loop index: {}", i);
        let idx = rng.random_range(0..funcs.len());
        let func = &funcs[idx];
        let res = func();
        if res.is_ok() {
            println!("<<< test success");
        } else {
            eprintln!("<<< test failed");
        }
    }
}

fn test_create_open_del_object() -> TestResult {
    println!("test_create_open_del_object");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();

    // Flags for create: includes OVERWRITE
    let create_flags = TEE_DATA_FLAG_ACCESS_READ |        /* we can later read the oject */
                       TEE_DATA_FLAG_ACCESS_WRITE |       /* we can later write into the object */
                       TEE_DATA_FLAG_ACCESS_WRITE_META |  /* we can later destroy or rename the object */
                       TEE_DATA_FLAG_OVERWRITE; /* destroy existing object of same ID */

    // Flags for open: OVERWRITE is NOT valid for open
    let open_flags =
        TEE_DATA_FLAG_ACCESS_READ | TEE_DATA_FLAG_ACCESS_WRITE | TEE_DATA_FLAG_ACCESS_WRITE_META;

    let obj_id = "test_create_open_del_object";

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        create_flags,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    // close the object
    TEE_CloseObject(object);

    // open the object (use open_flags, NOT create_flags)
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        open_flags,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    // delete the object
    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
} // 增加单元测试

fn test_create_raw_object() -> TestResult {
    println!("test_create_raw_object");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ |		/* we can later read the oject */
			TEE_DATA_FLAG_ACCESS_WRITE |		/* we can later write into the object */
			TEE_DATA_FLAG_ACCESS_WRITE_META |	/* we can later destroy or rename the object */
			TEE_DATA_FLAG_OVERWRITE; /* destroy existing object of same ID */
    let obj_jd = "test_create_rawtest_object_create_object";
    // let init_data = "test_init_data";

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_jd.as_ptr() as *const _,
        obj_jd.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );

    // unsafe {
    //     let mut obj: u32 = 0x01;
    //     let obj_ptr = &mut object as *mut TEE_ObjectHandle;
    //     *obj_ptr = obj as TEE_ObjectHandle;
    // }
    // trace_print!("create_raw_object obj: {}", object as usize );

    println!(
        "create_raw_object result: {:#010x}, obj: {}",
        res, object as usize
    );

    let data = b"test_write_object_data";

    let res = TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());
    println!("write_object_data result: {:#010x}", res);

    // seek to the beginning of the object
    let res = TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    println!("seek_object_data result: {:#010x}", res);

    let mut read_data = vec![0u8; data.len()];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        data.len(),
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, data.len());
    assert_eq!(read_data, data);
    println!("read_object_data result: {:#010x}", res);
    println!(
        "read_object_data data: {:?}",
        String::from_utf8_lossy(&read_data)
    );

    // delete the object
    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
} // 增加单元测试

fn test_obj_basic_operations() -> TestResult {
    println!("test_obj_basic_operations");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ |		/* we can later read the oject */
			TEE_DATA_FLAG_ACCESS_WRITE |		/* we can later write into the object */
			TEE_DATA_FLAG_ACCESS_WRITE_META |	/* we can later destroy or rename the object */
			TEE_DATA_FLAG_OVERWRITE; /* destroy existing object of same ID */
    let obj_jd = "test_create_rawtest_object_create_object";
    // let init_data = "test_init_data";

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_jd.as_ptr() as *const _,
        obj_jd.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );

    println!(
        "create_raw_object result: {:#010x}, obj: {}",
        res, object as usize
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(!object.is_null());

    let data = b"test_write_object_data";

    let res = TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());
    println!("write_object_data result: {:#010x}", res);
    assert_eq!(res, TEE_SUCCESS);

    // seek to the beginning of the object
    let res = TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    println!("seek_object_data result: {:#010x}", res);
    assert_eq!(res, TEE_SUCCESS);

    let mut read_data = vec![0u8; data.len()];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        data.len(),
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, data.len());
    assert_eq!(read_data, data);
    println!("read_object_data result: {:#010x}", res);
    println!(
        "read_object_data data: {:?}",
        String::from_utf8_lossy(&read_data)
    );

    // close the object
    TEE_CloseObject(object);

    // seek after close cause error
    let res = TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    assert_eq!(res, TEE_ERROR_ITEM_NOT_FOUND);
    println!("seek_object_data result: {:#010x}", res);

    let obj_read_delete_flag =
        TEE_DATA_FLAG_ACCESS_READ | TEE_DATA_FLAG_ACCESS_WRITE | TEE_DATA_FLAG_ACCESS_WRITE_META; /* delete need this flag */

    // // reopen the object
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_jd.as_ptr() as *const _,
        obj_jd.len() as usize,
        obj_read_delete_flag,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    println!(
        "open_object result: {:#010x}, object: {}",
        res, object as usize
    );

    // read the object data
    let mut read_data = vec![0u8; data.len()];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        data.len(),
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, data.len());
    assert_eq!(read_data, data);
    println!(
        "read_object_data result: {:#010x}, data: {:?}",
        res,
        String::from_utf8_lossy(&read_data)
    );

    // close the object
    TEE_CloseObject(object);

    // open again
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_jd.as_ptr() as *const _,
        obj_jd.len() as usize,
        obj_read_delete_flag,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    println!(
        "open_object result: {:#010x}, object: {}",
        res, object as usize
    );

    // truncate the object data
    let res = TEE_TruncateObjectData(object, 8);
    assert_eq!(res, TEE_SUCCESS);
    println!("truncate_object_data result: {:#010x}", res);

    // close the object
    TEE_CloseObject(object);

    // reopen and check truncated data
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_jd.as_ptr() as *const _,
        obj_jd.len() as usize,
        obj_read_delete_flag,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    println!(
        "open_object result: {:#010x}, object: {}",
        res, object as usize
    );

    // read the truncated data
    let mut read_data = vec![0u8; 8];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        8,
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, 8);
    assert_eq!(read_data, &data[..8]);
    println!(
        "read_object_data result: {:#010x}, data: {:?}",
        res,
        String::from_utf8_lossy(&read_data)
    );

    // // close and delete the object
    TEE_CloseAndDeletePersistentObject(object);
    // close the object
    // // reopen failed
    // let res = TEE_OpenPersistentObject(
    //     TEE_STORAGE_PRIVATE,
    //     obj_jd.as_ptr() as *const _,
    //     obj_jd.len() as usize,
    //     obj_read_delete_flag,
    //     &mut object,
    // );
    // assert_eq!(res, TEE_ERROR_ITEM_NOT_FOUND);
    // println!("open_object result: {:#010x}, object: {}", res, object as usize);

    Ok(())
}

// ========== 压力测试 ==========
// #[test]
// fn test_create_raw_object_loop() {
//     // for i in 0..1 {
//     //     println!("test_create_raw_object_loop: {}", i);
//     //     test_create_raw_object();
//     // }
//     test_create_object_with_initial_data();
//     test_create_raw_object();
// }

// ========== 创建对象测试 ==========
fn test_create_object_with_initial_data() -> TestResult {
    println!(">>> test_create_object_with_initial_data");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_create_with_init_data";
    let initial_data = b"initial_data_content";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        initial_data.as_ptr() as *const _,
        initial_data.len(),
        &mut object,
    );

    assert_eq!(res, TEE_SUCCESS);
    assert!(!object.is_null());

    // 读取初始数据
    let mut read_data = vec![0u8; initial_data.len()];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        initial_data.len(),
        &mut read_data_len,
    );
    println!(
        "read_object_data result: {:#010x}, data_len: {}, data: {:?}",
        res,
        read_data_len,
        String::from_utf8_lossy(&read_data)
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, initial_data.len());
    assert_eq!(read_data, initial_data);

    TEE_CloseObject(object);

    // //reopen and delete
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_WRITE_META,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    // println!("open_object result: {:#010x}, object: {}", res, object as usize);

    // delete the object
    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_create_object_without_overwrite() -> TestResult {
    println!(">>> test_create_object_without_overwrite");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_create_without_overwrite";

    let obj_data_flag =
        TEE_DATA_FLAG_ACCESS_READ | TEE_DATA_FLAG_ACCESS_WRITE | TEE_DATA_FLAG_ACCESS_WRITE_META;
    // 注意：没有 TEE_DATA_FLAG_OVERWRITE

    // 第一次创建应该成功
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    TEE_CloseObject(object);

    // 第二次创建应该失败（对象已存在且没有 OVERWRITE 标志）
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    // 应该返回错误（可能是 TEE_ERROR_ACCESS_CONFLICT 或其他）
    assert_ne!(res, TEE_SUCCESS);
    println!(
        "create without overwrite (second time) result: {:#010x}",
        res
    );

    // 清理
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_WRITE_META,
        &mut object,
    );

    if res == TEE_SUCCESS {
        TEE_CloseAndDeletePersistentObject(object);
    }

    Ok(())
}

fn test_create_object_with_overwrite() -> TestResult {
    println!(">>> test_create_object_with_overwrite");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_create_with_overwrite";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    // 第一次创建
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    let data1 = b"first_data";
    let res = TEE_WriteObjectData(object, data1.as_ptr() as *const _, data1.len());
    println!("write_object_data result: {:#010x}", res);
    TEE_CloseObject(object);

    // 第二次创建（带 OVERWRITE 标志）应该成功
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    let data2 = b"second_data";
    TEE_WriteObjectData(object, data2.as_ptr() as *const _, data2.len());
    TEE_CloseObject(object);

    // 验证数据被覆盖
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ | TEE_DATA_FLAG_ACCESS_WRITE_META, // delete need this flag
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    let mut read_data = vec![0u8; data2.len()];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        data2.len(),
        &mut read_data_len,
    );
    assert_eq!(read_data, data2);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

// ========== 打开对象测试 ==========

fn test_open_nonexistent_object() -> TestResult {
    println!(">>> test_open_nonexistent_object");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_nonexistent_object_12345";

    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ,
        &mut object,
    );

    assert_eq!(res, TEE_ERROR_ITEM_NOT_FOUND);
    assert!(object.is_null());

    Ok(())
}

fn test_open_object_different_flags() -> TestResult {
    println!(">>> test_open_object_different_flags");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_open_different_flags";

    // 创建对象
    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    TEE_CloseObject(object);

    // 用只读标志打开
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    TEE_CloseObject(object);

    // 用读写标志打开
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ | TEE_DATA_FLAG_ACCESS_WRITE,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    TEE_CloseObject(object);

    // 清理
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_WRITE_META,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

// ========== 写入数据测试 ==========
fn test_write_data_append() -> TestResult {
    println!(">>> test_write_data_append");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_write_append";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    // 写入第一段数据
    let data1 = b"first_part_";
    let res = TEE_WriteObjectData(object, data1.as_ptr() as *const _, data1.len());
    assert_eq!(res, TEE_SUCCESS);

    // 写入第二段数据（追加）
    let data2 = b"second_part";
    let res = TEE_WriteObjectData(object, data2.as_ptr() as *const _, data2.len());
    assert_eq!(res, TEE_SUCCESS);

    // 读取完整数据
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let total_len = data1.len() + data2.len();
    let mut read_data = vec![0u8; total_len];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        total_len,
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, total_len);
    assert_eq!(&read_data[..data1.len()], data1);
    assert_eq!(&read_data[data1.len()..], data2);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_write_data_large() -> TestResult {
    println!(">>> test_write_data_large");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_write_large";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    // 写入较大的数据（1KB）
    let large_data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
    let res = TEE_WriteObjectData(object, large_data.as_ptr() as *const _, large_data.len());
    assert_eq!(res, TEE_SUCCESS);

    // 读取并验证
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; large_data.len()];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        large_data.len(),
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, large_data.len());
    assert_eq!(read_data, large_data);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

// ========== 读取数据测试 ==========
fn test_read_data_partial() -> TestResult {
    println!(">>> test_read_data_partial");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_read_partial";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"abcdefghijklmnopqrstuvwxyz";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 从开头读取部分数据
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; 10];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        10,
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, 10);
    assert_eq!(&read_data[..10], &data[..10]);

    // 从中间位置读取
    TEE_SeekObjectData(object, 10, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; 5];
    let mut read_data_len = 0;
    let res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        5,
        &mut read_data_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(read_data_len, 5);
    assert_eq!(&read_data[..5], &data[10..15]);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_read_data_beyond_end() -> TestResult {
    println!(">>> test_read_data_beyond_end");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_read_beyond_end";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"test_data";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 尝试读取超过数据长度的数据
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; data.len() + 100];
    let mut read_data_len = 0;
    let _res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        data.len() + 100,
        &mut read_data_len,
    );
    // 应该只读取实际存在的数据
    assert_eq!(read_data_len, data.len());
    assert_eq!(&read_data[..data.len()], data);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

// ========== Seek 操作测试 ==========
fn test_seek_set() -> TestResult {
    println!(">>> test_seek_set");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_seek_set";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"0123456789abcdef";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 使用 SEEK_SET 定位到中间位置
    let res = TEE_SeekObjectData(object, 5, TEE_Whence::TEE_DATA_SEEK_SET);
    assert_eq!(res, TEE_SUCCESS);

    let mut read_data = vec![0u8; 5];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        5,
        &mut read_data_len,
    );
    assert_eq!(read_data_len, 5);
    assert_eq!(&read_data[..5], &data[5..10]);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_seek_cur() -> TestResult {
    println!(">>> test_seek_cur");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_seek_cur";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"0123456789abcdef";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 先读取一些数据
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; 5];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        5,
        &mut read_data_len,
    );

    // 使用 SEEK_CUR 向前移动
    let res = TEE_SeekObjectData(object, 3, TEE_Whence::TEE_DATA_SEEK_CUR);
    assert_eq!(res, TEE_SUCCESS);

    let mut read_data = vec![0u8; 5];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        5,
        &mut read_data_len,
    );
    assert_eq!(read_data_len, 5);
    assert_eq!(&read_data[..5], &data[8..13]);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_seek_end() -> TestResult {
    println!(">>> test_seek_end");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_seek_end";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"0123456789abcdef";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 使用 SEEK_END 从末尾向前定位
    let res = TEE_SeekObjectData(object, -5, TEE_Whence::TEE_DATA_SEEK_END);
    assert_eq!(res, TEE_SUCCESS);

    let mut read_data = vec![0u8; 5];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        5,
        &mut read_data_len,
    );
    assert_eq!(read_data_len, 5);
    assert_eq!(&read_data[..5], &data[data.len() - 5..]);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_seek_boundary() -> TestResult {
    println!(">>> test_seek_boundary");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_seek_boundary";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"test_data";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 定位到开头
    let res = TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    assert_eq!(res, TEE_SUCCESS);

    // 定位到末尾
    let res = TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_END);
    assert_eq!(res, TEE_SUCCESS);

    // 尝试读取（应该没有数据）
    let mut read_data = vec![0u8; 10];
    let mut read_data_len = 0;
    let _res = TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        10,
        &mut read_data_len,
    );
    // 在末尾读取应该返回 0 或错误
    assert_eq!(read_data_len, 0);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

// ========== 截断操作测试 ==========

fn test_truncate_to_zero() -> TestResult {
    println!(">>> test_truncate_to_zero");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_truncate_to_zero";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"test_data_to_truncate";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 截断到 0
    let res = TEE_TruncateObjectData(object, 0);
    assert_eq!(res, TEE_SUCCESS);

    // 验证数据为空
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; 100];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        100,
        &mut read_data_len,
    );
    assert_eq!(read_data_len, 0);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_truncate_to_middle() -> TestResult {
    println!(">>> test_truncate_to_middle");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_truncate_middle";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"abcdefghijklmnop";
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data.len());

    // 截断到中间位置
    let truncate_size = 8;
    let res = TEE_TruncateObjectData(object, truncate_size);
    assert_eq!(res, TEE_SUCCESS);

    // 验证截断后的数据
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; truncate_size];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        truncate_size,
        &mut read_data_len,
    );
    assert_eq!(read_data_len, truncate_size);
    assert_eq!(&read_data[..truncate_size], &data[..truncate_size]);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

fn test_truncate_to_end() -> TestResult {
    println!(">>> test_truncate_to_end");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_truncate_to_end";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);

    let data = b"test_data";
    let data_len = data.len();
    TEE_WriteObjectData(object, data.as_ptr() as *const _, data_len);

    // 截断到末尾（应该没有变化）
    let res = TEE_TruncateObjectData(object, data_len);
    assert_eq!(res, TEE_SUCCESS);

    // 验证数据未变
    TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
    let mut read_data = vec![0u8; data_len];
    let mut read_data_len = 0;
    TEE_ReadObjectData(
        object,
        read_data.as_mut_ptr() as *mut _,
        data_len,
        &mut read_data_len,
    );
    assert_eq!(read_data_len, data_len);
    assert_eq!(read_data, data);

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

// ========== 删除对象测试 ==========

fn test_delete_object() -> TestResult {
    println!(">>> test_delete_object");
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id = "test_delete_object";

    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;

    // 创建对象
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        core::ptr::null_mut(),
        core::ptr::null(),
        0,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    TEE_CloseObject(object);

    // 删除对象
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_WRITE_META,
        &mut object,
    );
    assert_eq!(res, TEE_SUCCESS);
    TEE_CloseAndDeletePersistentObject(object);

    // 验证对象已删除
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ,
        &mut object,
    );
    assert_eq!(res, TEE_ERROR_ITEM_NOT_FOUND);

    Ok(())
}

fn test_get_sm2_dsa_keypair_attributes() -> TestResult {
    println!(">>> test_alloc_transient_object");

    // alloc a transient object
    let mut object: TEE_ObjectHandle = core::ptr::null_mut();
    let res = TEE_AllocateTransientObject(TEE_TYPE_SM2_DSA_KEYPAIR, 256, &mut object);
    assert_eq!(res, TEE_SUCCESS);
    assert!(!object.is_null());

    // generate a sm2 dsa keypair
    let res = TEE_GenerateKey(object, 256, core::ptr::null(), 0);
    assert_eq!(res, TEE_SUCCESS);
    assert!(!object.is_null());

    // get private key attribute
    let mut private_key = vec![0u8; 32];
    let mut private_key_len = private_key.len() as usize;
    let res = TEE_GetObjectBufferAttribute(
        object,
        TEE_ATTR_ECC_PRIVATE_VALUE,
        private_key.as_mut_ptr() as *mut _,
        &mut private_key_len,
    );
    println!(
        "SM2 DSA get private_key result: {:#010x}, private_key: {:?}, private_key_len: {}",
        res,
        hex::encode(private_key),
        private_key_len
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(matches!(private_key_len, 31 | 32));

    // get public key x attribute
    let mut public_key_x = vec![0u8; 32];
    let mut public_key_x_len = public_key_x.len() as usize;
    let res = TEE_GetObjectBufferAttribute(
        object,
        TEE_ATTR_ECC_PUBLIC_VALUE_X,
        public_key_x.as_mut_ptr() as *mut _,
        &mut public_key_x_len,
    );
    println!(
        "SM2 DSA get public_key_x result: {:#010x}, public_key_x: {:?}, public_key_x_len: {}",
        res,
        hex::encode(public_key_x),
        public_key_x_len
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(matches!(public_key_x_len, 31 | 32));

    // get public key y attribute
    let mut public_key_y = vec![0u8; 32];
    let mut public_key_y_len = public_key_y.len() as usize;
    let res = TEE_GetObjectBufferAttribute(
        object,
        TEE_ATTR_ECC_PUBLIC_VALUE_Y,
        public_key_y.as_mut_ptr() as *mut _,
        &mut public_key_y_len,
    );
    println!(
        "SM2 DSA get public_key_y result: {:#010x}, public_key_y: {:?}, public_key_y_len: {}",
        res,
        hex::encode(public_key_y),
        public_key_y_len
    );

    assert_eq!(res, TEE_SUCCESS);
    assert!(matches!(public_key_y_len, 31 | 32));

    // get object info
    let mut object_info = TEE_ObjectInfo::default();
    let res = TEE_GetObjectInfo1(object, &mut object_info);
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(object_info.objectType, TEE_TYPE_SM2_DSA_KEYPAIR as u32);
    assert_eq!(object_info.objectUsage, TEE_USAGE_DEFAULT as u32);
    println!(
        "SM2 DSA get object info result: {:#010x}, object_info: {:#010X?}",
        res, object_info
    );

    // TEE_RestrictObjectUsage1
    let restrict_usage = object_info.objectUsage & !TEE_USAGE_EXTRACTABLE;
    let res = TEE_RestrictObjectUsage1(object, restrict_usage);
    assert_eq!(res, TEE_SUCCESS);

    // recheck object info
    let res = TEE_GetObjectInfo1(object, &mut object_info);
    assert_eq!(res, TEE_SUCCESS);
    assert_eq!(object_info.objectUsage, restrict_usage);

    // get private key attribute failed, cause TEE_GetObjectBufferAttribute panic,
    // so mask this test case, unmask it to test the panic

    // let mut private_key = vec![0u8; 32];
    // let mut private_key_len = private_key.len() as usize;
    // let res = TEE_GetObjectBufferAttribute(
    //     object,
    //     TEE_ATTR_ECC_PRIVATE_VALUE,
    //     private_key.as_mut_ptr() as *mut _,
    //     &mut private_key_len,
    // );
    // println!(
    //     "SM2 DSA get private_key after restrict usage result: {:#010x}, private_key: {:?}, private_key_len: {}",
    //     res,
    //     hex::encode(private_key),
    //     private_key_len
    // );

    // get public key x attribute success
    let mut public_key_x = vec![0u8; 32];
    let mut public_key_x_len = public_key_x.len() as usize;
    let res = TEE_GetObjectBufferAttribute(
        object,
        TEE_ATTR_ECC_PUBLIC_VALUE_X,
        public_key_x.as_mut_ptr() as *mut _,
        &mut public_key_x_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(matches!(public_key_x_len, 31 | 32));

    TEE_CloseObject(object);

    Ok(())
}

fn test_generate_persistant_sm2_dsa_keypair() -> TestResult {
    println!(">>> test_generate_persistant_sm2_dsa_keypair");

    // alloc a transient object
    let mut derive_key: TEE_ObjectHandle = core::ptr::null_mut();
    let res = TEE_AllocateTransientObject(TEE_TYPE_SM2_DSA_KEYPAIR, 256, &mut derive_key);
    assert_eq!(res, TEE_SUCCESS);
    assert!(!derive_key.is_null());

    // generate a sm2 dsa keypair
    let res = TEE_GenerateKey(derive_key, 256, core::ptr::null(), 0);
    assert_eq!(res, TEE_SUCCESS);
    assert!(!derive_key.is_null());

    // create a persistent object
    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_SHARE_READ
        | TEE_DATA_FLAG_SHARE_WRITE
        | TEE_DATA_FLAG_OVERWRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META;

    let obj_id = "test_generate_persistant_sm2_dsa_keypair";

    let mut key_object: TEE_ObjectHandle = core::ptr::null_mut();
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag,
        derive_key,
        core::ptr::null(),
        0,
        &mut key_object,
    );
    assert_eq!(res, TEE_SUCCESS);

    TEE_CloseObject(derive_key);

    // open the key object
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len() as usize,
        obj_data_flag & !TEE_DATA_FLAG_OVERWRITE,
        &mut key_object,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(!key_object.is_null());
    println!(
        "TEE_OpenPersistentObject res: {:#010X?}, key_object: {:#010X?}",
        res, key_object
    );

    // get the public key x attribute
    let mut public_key_x = vec![0u8; 32];
    let mut public_key_x_len = public_key_x.len() as usize;
    let res = TEE_GetObjectBufferAttribute(
        key_object,
        TEE_ATTR_ECC_PUBLIC_VALUE_X,
        public_key_x.as_mut_ptr() as *mut _,
        &mut public_key_x_len,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(matches!(public_key_x_len, 31 | 32));

    TEE_CloseAndDeletePersistentObject(key_object);
    Ok(())
}

fn test_enum_single_object() -> TestResult {
    println!("test_enum_single_object");

    // create two objects
    let mut object1: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id1 = "test_enum_2_object_object1";
    let obj_data1 = b"object1_data";
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id1.as_ptr() as *const _,
        obj_id1.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ | TEE_DATA_FLAG_ACCESS_WRITE | TEE_DATA_FLAG_ACCESS_WRITE_META,
        core::ptr::null_mut(),
        obj_data1.as_ptr() as *const _,
        obj_data1.len() as usize,
        &mut object1,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(!object1.is_null());

    // delete the objects
    TEE_CloseAndDeletePersistentObject(object1);

    Ok(())
}

fn test_create_two_objects() -> TestResult {
    println!("test_create_two_objects");

    // create two objects
    let mut object1: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id1 = "test_enum_2_object_object1";
    let obj_data1 = b"object1_data";
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id1.as_ptr() as *const _,
        obj_id1.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ
            | TEE_DATA_FLAG_ACCESS_WRITE
            | TEE_DATA_FLAG_ACCESS_WRITE_META
            | TEE_DATA_FLAG_OVERWRITE,
        core::ptr::null_mut(),
        obj_data1.as_ptr() as *const _,
        obj_data1.len() as usize,
        &mut object1,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(!object1.is_null());

    let mut object2: TEE_ObjectHandle = core::ptr::null_mut();
    let obj_id2 = "test_enum_2_object_object2";
    let obj_data2 = b"object2_data";
    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id2.as_ptr() as *const _,
        obj_id2.len() as usize,
        TEE_DATA_FLAG_ACCESS_READ
            | TEE_DATA_FLAG_ACCESS_WRITE
            | TEE_DATA_FLAG_ACCESS_WRITE_META
            | TEE_DATA_FLAG_OVERWRITE,
        core::ptr::null_mut(),
        obj_data2.as_ptr() as *const _,
        obj_data2.len() as usize,
        &mut object2,
    );
    assert_eq!(res, TEE_SUCCESS);
    assert!(!object2.is_null());

    // // enum the objects
    // let mut object_enum: TEE_ObjectEnumHandle = core::ptr::null_mut();
    // let res = TEE_AllocatePersistentObjectEnumerator(&mut object_enum);
    // assert_eq!(res, TEE_SUCCESS);

    // // free the object enumerator
    // TEE_FreePersistentObjectEnumerator(object_enum);

    // delete the objects
    TEE_CloseAndDeletePersistentObject(object1);
    TEE_CloseAndDeletePersistentObject(object2);

    Ok(())
}

fn test_create_some_objects() -> TestResult {
    println!("test_create_some_objects");

    for file_idx in 0..10 {
        // create object with file_idx as obj_id
        let mut object: TEE_ObjectHandle = core::ptr::null_mut();
        let obj_id = format!("test_create_some_objects_object_{}", file_idx);
        // random obj data, with random length from 1 to 4096
        let mut obj_data = vec![0u8; rand::rng().random_range(1..4096)];
        // fill obj_data with random data
        for i in 0..obj_data.len() {
            obj_data[i] = rand::rng().random_range(0..256) as u8;
        }
        let res = TEE_CreatePersistentObject(
            TEE_STORAGE_PRIVATE,
            obj_id.as_ptr() as *const _,
            obj_id.len() as usize,
            TEE_DATA_FLAG_ACCESS_READ
                | TEE_DATA_FLAG_ACCESS_WRITE
                | TEE_DATA_FLAG_ACCESS_WRITE_META
                | TEE_DATA_FLAG_OVERWRITE,
            core::ptr::null_mut(),
            obj_data.as_ptr() as *const _,
            obj_data.len() as usize,
            &mut object,
        );
        assert_eq!(res, TEE_SUCCESS);
        assert!(!object.is_null());

        TEE_CloseAndDeletePersistentObject(object);
    }
    Ok(())
}

fn test_create_open_read_some_objects() -> TestResult {
    println!("test_create_open_read_some_objects");

    for file_idx in 0..10 {
        println!("test_create_open_read_some_objects_file_idx: {}", file_idx);
        // create object with file_idx as obj_id
        let mut object: TEE_ObjectHandle = core::ptr::null_mut();
        let obj_id = format!("test_create_open_read_some_objects_{}", file_idx);
        // random obj data, with random length from 1 to 4096
        let mut obj_create_data = vec![0u8; rand::rng().random_range(1..4096)];
        // fill obj_data with random data
        for i in 0..obj_create_data.len() {
            obj_create_data[i] = rand::rng().random_range(0..256) as u8;
        }
        let res = TEE_CreatePersistentObject(
            TEE_STORAGE_PRIVATE,
            obj_id.as_ptr() as *const _,
            obj_id.len() as usize,
            TEE_DATA_FLAG_ACCESS_READ
                | TEE_DATA_FLAG_ACCESS_WRITE
                | TEE_DATA_FLAG_ACCESS_WRITE_META
                | TEE_DATA_FLAG_OVERWRITE,
            core::ptr::null_mut(),
            obj_create_data.as_ptr() as *const _,
            obj_create_data.len() as usize,
            &mut object,
        );
        assert_eq!(res, TEE_SUCCESS);
        assert!(!object.is_null());

        // // close the object
        TEE_CloseObject(object);

        // // open the object
        let res = TEE_OpenPersistentObject(
            TEE_STORAGE_PRIVATE,
            obj_id.as_ptr() as *const _,
            obj_id.len() as usize,
            TEE_DATA_FLAG_ACCESS_READ
                | TEE_DATA_FLAG_ACCESS_WRITE
                | TEE_DATA_FLAG_ACCESS_WRITE_META,
            &mut object,
        );
        assert_eq!(res, TEE_SUCCESS);
        assert!(!object.is_null());

        // read the object data
        let mut obj_read_data = vec![0u8; obj_create_data.len()];
        let mut obj_read_data_len = 0;
        let res = TEE_ReadObjectData(
            object,
            obj_read_data.as_mut_ptr() as *mut _,
            obj_create_data.len(),
            &mut obj_read_data_len,
        );
        assert_eq!(res, TEE_SUCCESS);
        assert_eq!(obj_read_data_len, obj_create_data.len());
        assert_eq!(obj_read_data, obj_create_data);

        // seek to the end of the object
        let res = TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_END);
        assert_eq!(res, TEE_SUCCESS);

        // write some random data to the object with random length from 1 to 512
        let mut obj_write_data = vec![0u8; rand::rng().random_range(1..512)];
        // fill obj_write_data with random data
        for i in 0..obj_write_data.len() {
            obj_write_data[i] = rand::rng().random_range(0..256) as u8;
        }
        let res = TEE_WriteObjectData(
            object,
            obj_write_data.as_ptr() as *const _,
            obj_write_data.len(),
        );
        assert_eq!(res, TEE_SUCCESS);

        // read all the object data
        // seek to the start of the object
        let res = TEE_SeekObjectData(object, 0, TEE_Whence::TEE_DATA_SEEK_SET);
        assert_eq!(res, TEE_SUCCESS);

        let mut obj_read_data = vec![0u8; obj_create_data.len() + obj_write_data.len()];
        let mut obj_read_data_len = 0;
        let res = TEE_ReadObjectData(
            object,
            obj_read_data.as_mut_ptr() as *mut _,
            obj_read_data.len(),
            &mut obj_read_data_len,
        );
        assert_eq!(res, TEE_SUCCESS);
        assert_eq!(
            obj_read_data_len,
            obj_create_data.len() + obj_write_data.len()
        );
        assert_eq!(
            obj_read_data,
            [&obj_create_data[..], &obj_write_data[..]].concat()
        );

        // close the object
        TEE_CloseAndDeletePersistentObject(object);
    }
    Ok(())
}

fn test_enum_objects() -> TestResult {
    println!("test_enum_objects");

    let num_objs = 10;

    debug_assert!(num_objs >= 2);
    // create some objects
    for file_idx in 0..num_objs {
        // create object with file_idx as obj_id
        let mut object: TEE_ObjectHandle = core::ptr::null_mut();
        let obj_id = format!("test_enum_objects_object_{}", file_idx);
        let obj_data = format!("object_data_{}", file_idx);

        let res = TEE_CreatePersistentObject(
            TEE_STORAGE_PRIVATE,
            obj_id.as_ptr() as *const _,
            obj_id.len() as usize,
            TEE_DATA_FLAG_ACCESS_READ
                | TEE_DATA_FLAG_ACCESS_WRITE
                | TEE_DATA_FLAG_ACCESS_WRITE_META
                | TEE_DATA_FLAG_OVERWRITE,
            core::ptr::null_mut(),
            obj_data.as_ptr() as *const _,
            obj_data.len() as usize,
            &mut object,
        );
        assert_eq!(res, TEE_SUCCESS);
        assert!(!object.is_null());

        // close the object
        TEE_CloseObject(object);
    }

    // enum the objects
    let mut object_enum: TEE_ObjectEnumHandle = core::ptr::null_mut();
    let res = TEE_AllocatePersistentObjectEnumerator(&mut object_enum);
    assert_eq!(res, TEE_SUCCESS);

    // start the object enumerator
    let res = TEE_StartPersistentObjectEnumerator(object_enum, TEE_STORAGE_PRIVATE);
    assert_eq!(res, TEE_SUCCESS);

    // get the next object
    let mut restarted_flag = false;
    for i in 0.. {
        println!("get the next object with index: {}", i);
        let mut object_info: TEE_ObjectInfo = TEE_ObjectInfo::default();
        let mut object_id = vec![0u8; 1024];
        let mut object_id_len = object_id.len();
        let res = TEE_GetNextPersistentObject(
            object_enum,
            &mut object_info,
            object_id.as_mut_ptr() as *mut _,
            &mut object_id_len,
        );

        if res == TEE_ERROR_ITEM_NOT_FOUND {
            println!(
                "no more objects, return value: {:#010X?}, break the loop",
                res
            );
            break;
        }

        if res != TEE_SUCCESS {
            panic!(
                "get next object failed, return value: {:#010X?}, break the loop",
                res
            );
        }

        if !restarted_flag && i == num_objs / 2 {
            restarted_flag = true;
            // 50% chance to restart the object enumerator
            if rand::rng().random_range(0..2) == 0 {
                println!("restart the object enumerator");
                TEE_ResetPersistentObjectEnumerator(object_enum);
                TEE_StartPersistentObjectEnumerator(object_enum, TEE_STORAGE_PRIVATE);
            }
        }
    }

    // free the object enumerator
    TEE_FreePersistentObjectEnumerator(object_enum);

    // delete all objects
    for file_idx in 0..num_objs {
        let mut object: TEE_ObjectHandle = core::ptr::null_mut();

        let obj_id = format!("test_enum_objects_object_{}", file_idx);
        let res = TEE_OpenPersistentObject(
            TEE_STORAGE_PRIVATE,
            obj_id.as_ptr() as *const _,
            obj_id.len() as usize,
            TEE_DATA_FLAG_ACCESS_WRITE_META,
            &mut object,
        );
        assert_eq!(res, TEE_SUCCESS);
        assert!(!object.is_null());

        TEE_CloseAndDeletePersistentObject(object);
    }

    Ok(())
}
