use amc_admission::{ledger::Ledger, store::Store};

#[test]
fn durable_state_has_a_single_owner_and_survives_reopen() {
    let root = std::env::temp_dir().join(format!(
        "amc-store-{}-{}",
        std::process::id(),
        amc_admission::store::fresh_id().unwrap()
    ));
    let (store, ledger) = Store::open(&root, "boot").unwrap();
    store.save(&ledger).unwrap();
    assert!(Store::open(&root, "boot").is_err());
    drop(store);
    let (store, restored) = Store::open(&root, "boot").unwrap();
    assert_eq!(
        serde_json::to_value(&ledger).unwrap(),
        serde_json::to_value(&restored).unwrap()
    );
    drop(store);
    std::fs::write(root.join("ledger.json"), b"truncated").unwrap();
    assert!(Store::open(&root, "boot").is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn new_boot_discards_only_previous_boot_reservations() {
    let root = std::env::temp_dir().join(format!(
        "amc-boot-{}",
        amc_admission::store::fresh_id().unwrap()
    ));
    let (store, _) = Store::open(&root, "old").unwrap();
    store.save(&Ledger::new("old".into())).unwrap();
    drop(store);
    let (store, ledger) = Store::open(&root, "new").unwrap();
    assert_eq!(ledger.boot_id, "new");
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
