use amc_supervision::{
    policy::Policy,
    recovery::{Identity, Phase},
    store::{Store, atomic_json},
};
use std::os::unix::fs::PermissionsExt;

const BOOT: &str = "a0e7b901-008c-479c-bf78-3fd6a6fb408d";

#[test]
fn recovery_is_single_owner_and_reboot_or_missing_state_cannot_forgive_an_attempt() {
    let root = std::env::temp_dir().join(format!(
        "amc-recovery-{}",
        amc_admission::store::fresh_id().unwrap()
    ));
    let p: Policy =
        serde_json::from_str(include_str!("../../../examples/supervision.json")).unwrap();
    p.validate().unwrap();
    let (store, mut state) = Store::open(&root, BOOT).unwrap();
    state
        .begin(
            p.domains[0].clone(),
            Identity {
                invocation: "a".repeat(32),
                cgroup: "/example-backend.service".into(),
                inode: 1,
                pid: 1,
                start_ticks: 1,
            },
            100,
            100,
            &p,
        )
        .unwrap();
    store.save(&state).unwrap();
    assert!(Store::open(&root, BOOT).is_err());
    drop(store);
    let (store, state) = Store::open(&root, "ed447a46-39ce-496e-b58c-c6e029e23768").unwrap();
    assert_eq!(state.active.unwrap().phase, Phase::Tripped);
    assert_eq!(state.attempts.len(), 1);
    assert_eq!(
        std::fs::metadata(root.join("recovery.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(store);
    std::fs::remove_file(root.join("recovery.json")).unwrap();
    assert!(Store::open(&root, BOOT).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn atomic_publication_sets_explicit_modes_and_refuses_oversized_state() {
    let root = std::env::temp_dir().join(format!(
        "amc-publication-{}",
        amc_admission::store::fresh_id().unwrap()
    ));
    std::fs::create_dir(&root).unwrap();
    let public = root.join("health.json");
    atomic_json(&public, &serde_json::json!({"inhibit":true}), 0o644, false).unwrap();
    assert_eq!(
        std::fs::metadata(&public).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(atomic_json(&public, &"x".repeat(1_048_577), 0o644, false).is_err());
    assert_eq!(
        std::fs::read_to_string(&public).unwrap(),
        "{\"inhibit\":true}"
    );
    std::fs::remove_dir_all(root).unwrap();
}
