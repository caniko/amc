use amc_admission::{host_native, ledger::ClientIdentity};

#[test]
fn delegated_preparation_requires_the_same_live_host_uid_and_start_time() {
    let pid = std::process::id() as i32;
    let uid = nix::unistd::geteuid().as_raw();
    let mut origin = ClientIdentity {
        pid,
        start_ticks: host_native::process_start(pid).unwrap(),
    };
    assert_eq!(host_native::prepared_origin(&origin, uid).unwrap(), pid);
    assert!(host_native::prepared_origin(&origin, uid + 1).is_err());
    origin.start_ticks += 1;
    assert!(host_native::prepared_origin(&origin, uid).is_err());
    origin.pid = 0;
    assert!(host_native::prepared_origin(&origin, uid).is_err());
}
