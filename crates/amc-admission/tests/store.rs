use amc_admission::{
    ledger::{Contract, Entry, Identity, Ledger, Phase},
    store::Store,
};

const BOOT: &str = "12345678-1234-4abc-8def-123456789abc";
const NEXT_BOOT: &str = "87654321-4321-4abc-8def-cba987654321";

fn running_ledger() -> Ledger {
    let mut ledger = Ledger::new(BOOT.into());
    ledger.entries.push(Entry {
        id: amc_admission::store::fresh_id().unwrap(),
        name: "tool".into(),
        contract: Contract {
            slice: "agent-tools.slice".into(),
            memory_max: 600,
            memory_swap_max: 0,
            max_running: 1,
            pause_file: None,
            burst: false,
            runtime_max_sec: None,
        },
        phase: Phase::Running,
        deadline_ms: 0,
        identity: Some(Identity {
            invocation: "0123456789abcdef0123456789abcdef".into(),
            cgroup: "/test/job.service".into(),
            inode: 1,
        }),
        client: None,
    });
    ledger
}

#[test]
fn durable_state_has_a_single_owner_and_survives_reopen() {
    let root = std::env::temp_dir().join(format!(
        "amc-store-{}-{}",
        std::process::id(),
        amc_admission::store::fresh_id().unwrap()
    ));
    let (store, _) = Store::open(&root, BOOT).unwrap();
    let ledger = running_ledger();
    store.save(&ledger).unwrap();
    assert!(Store::open(&root, BOOT).is_err());
    drop(store);
    let (store, restored) = Store::open(&root, BOOT).unwrap();
    assert_eq!(
        serde_json::to_value(&ledger).unwrap(),
        serde_json::to_value(&restored).unwrap()
    );
    drop(store);
    std::fs::write(root.join("ledger.json"), b"truncated").unwrap();
    assert!(Store::open(&root, BOOT).is_err());
    std::fs::remove_file(root.join("ledger.json")).unwrap();
    assert!(Store::open(&root, BOOT).is_err());
    nix::unistd::mkfifo(
        &root.join("ledger.json"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    assert!(Store::open(&root, BOOT).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn new_boot_discards_only_previous_boot_reservations() {
    let root = std::env::temp_dir().join(format!(
        "amc-boot-{}",
        amc_admission::store::fresh_id().unwrap()
    ));
    let (store, _) = Store::open(&root, BOOT).unwrap();
    store.save(&running_ledger()).unwrap();
    drop(store);
    let (store, ledger) = Store::open(&root, NEXT_BOOT).unwrap();
    assert_eq!(ledger.boot_id, NEXT_BOOT);
    assert!(ledger.entries.is_empty());
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_boot_identity_cannot_discard_reservations() {
    let root = std::env::temp_dir().join(format!(
        "amc-corrupt-boot-{}",
        amc_admission::store::fresh_id().unwrap()
    ));
    let (store, _) = Store::open(&root, BOOT).unwrap();
    let ledger = running_ledger();
    store.save(&ledger).unwrap();
    drop(store);
    let path = root.join("ledger.json");
    for invalid in ["", "boot", "12345678-1234-4ABC-8DEF-123456789ABC"] {
        let mut data = serde_json::to_value(&ledger).unwrap();
        data["boot_id"] = invalid.into();
        let bytes = serde_json::to_vec(&data).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(Store::open(&root, BOOT).is_err(), "stored ID: {invalid}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();
        assert!(
            Store::open(&root, invalid).is_err(),
            "current ID: {invalid}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
