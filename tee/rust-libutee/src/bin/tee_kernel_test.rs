use rust_utee::syscalls::syscall_table::_utee_test;

fn main() {
    let res = unsafe { _utee_test() };
    assert_eq!(res, 0);
}
