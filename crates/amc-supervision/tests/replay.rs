use amc_supervision::{native::Boundary, replay};
use serde_json::{Value, json};
use std::io::Cursor;

fn frame(boot: &str, ms: u64, observed: bool) -> Value {
    json!({
        "version": 1, "boot_id": boot, "observed_boot_ms": ms,
        "policy": {
            "version": 1, "mode": "shadow", "forecast_recovery": false,
            "reserve_bytes": 100, "emergency_available_bytes": 20, "emergency_full_psi": 0.1,
            "term_ms": 1000, "kill_ms": 1000, "cooldown_ms": 1000, "recovery_window_ms": 60000,
            "domain_recovery_limit": 2, "host_recovery_limit": 3,
            "forecast": {"horizon": 10, "calibration": 32, "alpha": 0.1, "max_gap_ms": 1500},
            "domains": [{"id": "backend", "uid": null, "unit": "backend.service", "lifecycle": "restart",
                "memory_max": 1000, "memory_swap_max": 0, "priority": 10}], "job_pools": []
        },
        "domains": if observed {vec![json!({"id": "backend", "status": "observed",
            "identity": {"invocation": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "cgroup": "/fixture",
                "inode": 1, "pid": 1, "start_ticks": 1},
            "boundaries": [Boundary { key: "memory.current".into(), cgroup: "/fixture".into(), inode: 1,
                current_bytes: 100 + ms / 1000, limit_bytes: 1000 }]})]} else {vec![]}
    })
}

fn trace(frames: &[Value]) -> String {
    frames
        .iter()
        .map(|f| format!("{}\n", serde_json::to_string(f).unwrap()))
        .collect()
}

const BOOT: &str = "a0e7b901-008c-479c-bf78-3fd6a6fb408d";
const NEXT_BOOT: &str = "ed447a46-39ce-496e-b58c-c6e029e23768";

#[test]
fn missing_domain_frames_censor_the_whole_window_even_with_a_short_gap() {
    let frames = [
        frame(BOOT, 0, true),
        frame(BOOT, 250, false),
        frame(BOOT, 1000, true),
    ];
    let summary = replay::run(Cursor::new(trace(&frames))).unwrap();
    assert_eq!(summary["backend"].observations, 2);
    assert_eq!(summary["backend"].censored_windows, 1);
}

#[test]
fn boot_and_policy_changes_preserve_all_completed_and_censored_window_counts() {
    let mut frames: Vec<_> = (0..50).map(|t| frame(BOOT, t * 1000, true)).collect();
    frames.extend((0..50).map(|t| frame(NEXT_BOOT, t * 1000, true)));
    for t in 50..100 {
        let mut f = frame(NEXT_BOOT, t * 1000, true);
        f["policy"]["forecast"]["calibration"] = json!(64);
        frames.push(f);
    }
    let summary = replay::run(Cursor::new(trace(&frames))).unwrap();
    assert_eq!(summary["backend"].observations, 150);
    assert_eq!(summary["backend"].completed_windows, 120);
    assert_eq!(summary["backend"].censored_windows, 20);
    assert_eq!(summary["backend"].evaluated_windows, 0);
}

#[test]
fn interleaved_boots_and_duplicate_domains_are_rejected() {
    let frames = [
        frame(BOOT, 0, true),
        frame(NEXT_BOOT, 0, true),
        frame(BOOT, 1000, true),
    ];
    assert!(replay::run(Cursor::new(trace(&frames))).is_err());
    let mut f = frame(BOOT, 0, true);
    let domain = f["domains"][0].clone();
    f["domains"].as_array_mut().unwrap().push(domain);
    assert!(replay::run(Cursor::new(trace(&[f]))).is_err());
}
