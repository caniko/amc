//! Namespace callers cannot use systemctl's implicit private-manager transport.
//! Query only the required public properties through the exact host session bus.
use super::{Cleanup, UnitState, capture, safe_cgroup_path, terminated, valid_unit};
use serde_json::Value;
use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn command(uid: u32) -> Command {
    let mut command = Command::new("busctl");
    command.args([
        format!("--address=unix:path=/run/user/{uid}/bus"),
        "--timeout=2s".into(),
        "--json=short".into(),
        "--".into(),
    ]);
    command
}

fn manager_call(uid: u32, method: &str, signature: &str, arguments: &[&str]) -> Option<Value> {
    let text = capture(
        command(uid)
            .args([
                "call",
                "org.freedesktop.systemd1",
                "/org/freedesktop/systemd1",
                "org.freedesktop.systemd1.Manager",
                method,
                signature,
            ])
            .args(arguments),
    )
    .ok()?;
    serde_json::from_str(&text).ok()
}

fn properties(uid: u32, path: &str, interface: &str, names: &[&str]) -> Option<Vec<Value>> {
    let text = capture(
        command(uid)
            .args(["get-property", "org.freedesktop.systemd1", path, interface])
            .args(names),
    )
    .ok()?;
    let values = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<Value>, _>>()
        .ok()?;
    (values.len() == names.len()).then_some(values)
}

fn data<'a>(property: &'a Value, kind: &str) -> Option<&'a Value> {
    (property.get("type")?.as_str()? == kind).then(|| property.get("data"))?
}

pub(super) fn query(uid: u32, unit: &str) -> Option<UnitState> {
    match observe(uid, unit)? {
        Snapshot::Coherent(state) => Some(state),
        Snapshot::Transition => None,
    }
}

enum Snapshot {
    Coherent(UnitState),
    Transition,
}

fn observe(uid: u32, unit: &str) -> Option<Snapshot> {
    if !valid_unit(unit) {
        return None;
    }
    // Like systemctl show, LoadUnit returns a not-found object after collection.
    // A failed call remains unknown; it is never interpreted as absence.
    let loaded = manager_call(uid, "LoadUnit", "s", &[unit])?;
    let path = data(&loaded, "o")?.as_array()?.first()?.as_str()?;
    if !path.starts_with("/org/freedesktop/systemd1/unit/") {
        return None;
    }
    snapshot(unit, |interface, names| {
        properties(uid, path, interface, names)
    })
}

fn snapshot(
    unit: &str,
    mut properties: impl FnMut(&str, &[&str]) -> Option<Vec<Value>>,
) -> Option<Snapshot> {
    const UNIT_PROPERTIES: &[&str] = &["Id", "LoadState", "ActiveState", "InvocationID"];
    let unit_fields = properties("org.freedesktop.systemd1.Unit", UNIT_PROPERTIES)?;
    let mut state = unit_state(unit, &unit_fields)?;
    if state.loaded {
        let service = properties(
            "org.freedesktop.systemd1.Service",
            &[
                "ExecMainStartTimestampMonotonic",
                "MainPID",
                "ExecMainCode",
                "ExecMainStatus",
                "ControlGroup",
            ],
        )?;
        state.started = data(&service[0], "t")?.as_u64()? > 0;
        state.main_pid = u32::try_from(data(&service[1], "u")?.as_u64()?)
            .ok()
            .filter(|pid| *pid > 0);
        let code = data(&service[2], "i")?.as_i64()?;
        let status = i32::try_from(data(&service[3], "i")?.as_i64()?).ok()?;
        state.workload_code = match code {
            1 if (0..=255).contains(&status) => Some(status),
            2 | 3 if (1..=64).contains(&status) => Some(128 + status),
            _ => None,
        };
        state.control_group = Some(data(&service[4], "s")?.as_str()?.to_owned());
        // The interfaces are separate manager queries. A unit can start between
        // them, yielding an old inactive state with a new live MainPID. Never
        // pin or release startup from that mixed snapshot; retry under execute's
        // existing startup deadline. Invocation replacement is likewise unknown.
        if properties("org.freedesktop.systemd1.Unit", UNIT_PROPERTIES)? != unit_fields {
            return Some(Snapshot::Transition);
        }
    }
    Some(Snapshot::Coherent(state))
}

fn unit_state(unit: &str, fields: &[Value]) -> Option<UnitState> {
    if fields.len() != 4 || data(&fields[0], "s")?.as_str()? != unit {
        return None;
    }
    let loaded = match data(&fields[1], "s")?.as_str()? {
        "loaded" => true,
        "not-found" => false,
        _ => return None,
    };
    let stopped = match data(&fields[2], "s")?.as_str()? {
        "inactive" | "failed" => true,
        "active" | "activating" | "deactivating" | "reloading" => false,
        _ => return None,
    };
    let bytes = data(&fields[3], "ay")?.as_array()?;
    let invocation = if bytes.is_empty() {
        None
    } else {
        if bytes.len() != 16 {
            return None;
        }
        let bytes = bytes
            .iter()
            .map(|v| u8::try_from(v.as_u64()?).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(bytes.iter().map(|v| format!("{v:02x}")).collect())
    };
    Some(UnitState {
        loaded,
        stopped,
        invocation,
        started: false,
        control_group: None,
        main_pid: None,
        workload_code: None,
    })
}

pub(super) fn cleanup(uid: u32, unit: &str) -> Cleanup {
    if !valid_unit(unit) || manager_call(uid, "StopUnit", "ss", &[unit, "replace"]).is_none() {
        return Cleanup::Unknown;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = await_cleanup(
        || observe(uid, unit),
        terminated,
        || {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
            true
        },
    );
    if result == Cleanup::Stopped {
        let _ = manager_call(uid, "ResetFailedUnit", "s", &[unit]);
    }
    result
}

fn await_cleanup(
    mut observe: impl FnMut() -> Option<Snapshot>,
    terminated: impl Fn(Option<&str>) -> Option<bool>,
    mut retry: impl FnMut() -> bool,
) -> Cleanup {
    loop {
        match observe() {
            Some(Snapshot::Coherent(state))
                if state.loaded
                    && state.stopped
                    && terminated(state.control_group.as_deref()) == Some(true) =>
            {
                return Cleanup::Stopped;
            }
            Some(Snapshot::Coherent(state))
                if state.loaded
                    && !state.stopped
                    && state
                        .control_group
                        .as_deref()
                        .and_then(safe_cgroup_path)
                        .is_some() => {}
            // Only a positively detected manager transition is retryable.
            // Missing or invalid observations remain unknown immediately.
            Some(Snapshot::Transition) => {}
            _ => return Cleanup::Unknown,
        }
        if !retry() {
            return Cleanup::Unknown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_bus_identity_and_unknown_states_cannot_prove_completion() {
        let unit = "app-amc-job-session.service";
        let fields = vec![
            json!({"type":"s", "data":unit}),
            json!({"type":"s", "data":"loaded"}),
            json!({"type":"s", "data":"active"}),
            json!({"type":"ay", "data":vec![1;16]}),
        ];
        let state = unit_state(unit, &fields).unwrap();
        assert!(state.loaded && !state.stopped);
        assert_eq!(
            state.invocation.as_deref(),
            Some("01010101010101010101010101010101")
        );
        for (index, bad) in [
            (0, json!({"type":"s", "data":"foreign.service"})),
            (1, json!({"type":"s", "data":"error"})),
            (2, json!({"type":"s", "data":"unknown"})),
            (3, json!({"type":"ay", "data":vec![256;16]})),
            (3, json!({"type":"ay", "data":vec![1;15]})),
        ] {
            let mut changed = fields.clone();
            changed[index] = bad;
            assert!(unit_state(unit, &changed).is_none());
        }
    }

    #[test]
    fn startup_sampling_rejects_a_manager_transition_between_interfaces() {
        let unit = "app-amc-host-runner-transition.service";
        let unit_fields = |active: &str, invocation: u8| {
            vec![
                json!({"type":"s", "data":unit}),
                json!({"type":"s", "data":"loaded"}),
                json!({"type":"s", "data":active}),
                json!({"type":"ay", "data":vec![invocation;16]}),
            ]
        };
        let service = vec![
            json!({"type":"t", "data":1000}),
            json!({"type":"u", "data":123}),
            json!({"type":"i", "data":0}),
            json!({"type":"i", "data":0}),
            json!({"type":"s", "data":format!("/app.slice/{unit}")}),
        ];
        for (before, after, stable) in [
            (unit_fields("active", 1), unit_fields("active", 1), true),
            (unit_fields("inactive", 1), unit_fields("active", 1), false),
            (unit_fields("active", 1), unit_fields("active", 2), false),
        ] {
            let mut responses = [before, service.clone(), after].into_iter();
            let state = snapshot(unit, |_, _| responses.next());
            if stable {
                let Some(Snapshot::Coherent(state)) = state else {
                    panic!("a stable native snapshot remains observable");
                };
                assert!(state.started && !state.stopped);
                assert_eq!(state.main_pid, Some(123));
            } else {
                assert!(
                    matches!(state, Some(Snapshot::Transition)),
                    "mixed manager observations must be retried"
                );
            }
        }
    }

    #[test]
    fn cleanup_retries_a_transition_until_stopped_and_empty() {
        let stopped = UnitState {
            loaded: true,
            stopped: true,
            invocation: Some("01".repeat(16)),
            started: true,
            control_group: Some("/app.slice/app-amc-cleanup.service".into()),
            main_pid: None,
            workload_code: Some(0),
        };
        let mut observations = [
            Some(Snapshot::Transition),
            Some(Snapshot::Coherent(stopped)),
        ]
        .into_iter();
        let mut retries = 0;
        let result = await_cleanup(
            || {
                observations
                    .next()
                    .expect("cleanup must consume bounded observations")
            },
            |path| {
                assert_eq!(path, Some("/app.slice/app-amc-cleanup.service"));
                Some(true)
            },
            || {
                retries += 1;
                true
            },
        );
        assert_eq!(result, Cleanup::Stopped);
        assert_eq!(retries, 1);
    }

    #[test]
    fn cleanup_transition_retries_are_deadline_bounded_and_unknown_fails_closed() {
        let mut observations = 0;
        let mut retries = 0;
        assert_eq!(
            await_cleanup(
                || {
                    observations += 1;
                    Some(Snapshot::Transition)
                },
                |_| panic!("incoherent observations cannot prove termination"),
                || {
                    retries += 1;
                    retries < 3
                },
            ),
            Cleanup::Unknown,
        );
        assert_eq!(observations, 3);
        assert_eq!(retries, 3);
        assert_eq!(
            await_cleanup(
                || None,
                |_| panic!("unavailable observations cannot prove termination"),
                || panic!("unavailable observations must not be retried"),
            ),
            Cleanup::Unknown,
        );
    }

    #[test]
    fn cleanup_never_accepts_a_stopped_unit_with_unconfirmed_descendants() {
        for empty in [None, Some(false)] {
            let result = await_cleanup(
                || {
                    Some(Snapshot::Coherent(UnitState {
                        loaded: true,
                        stopped: true,
                        invocation: Some("01".repeat(16)),
                        started: true,
                        control_group: Some("/app.slice/app-amc-cleanup.service".into()),
                        main_pid: None,
                        workload_code: Some(0),
                    }))
                },
                |_| empty,
                || panic!("a stopped unit with unconfirmed descendants must fail closed"),
            );
            assert_eq!(result, Cleanup::Unknown);
        }
    }
}
