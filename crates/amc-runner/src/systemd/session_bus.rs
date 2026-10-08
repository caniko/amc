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
    let unit_fields = properties(
        uid,
        path,
        "org.freedesktop.systemd1.Unit",
        &["Id", "LoadState", "ActiveState", "InvocationID"],
    )?;
    let mut state = unit_state(unit, &unit_fields)?;
    if state.loaded {
        let service = properties(
            uid,
            path,
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
    }
    Some(state)
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
    loop {
        match query(uid, unit) {
            Some(state)
                if state.loaded
                    && state.stopped
                    && terminated(state.control_group.as_deref()) == Some(true) =>
            {
                let _ = manager_call(uid, "ResetFailedUnit", "s", &[unit]);
                return Cleanup::Stopped;
            }
            Some(state)
                if state.loaded
                    && !state.stopped
                    && state
                        .control_group
                        .as_deref()
                        .and_then(safe_cgroup_path)
                        .is_some()
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25))
            }
            _ => return Cleanup::Unknown,
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
}
