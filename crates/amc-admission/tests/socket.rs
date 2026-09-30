use amc_admission::{
    ledger::{Contract, Entry, Headroom, Identity, Phase, Policy},
    native::Native,
    protocol::{Message, call},
    server::serve,
    store::fresh_id,
};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Fake {
    terminated: Arc<AtomicBool>,
}
impl Native for Fake {
    fn headroom(&self, _: &Contract, _: u64) -> Result<Headroom> {
        Ok(Headroom {
            shared_bytes: 2000,
            slice_bytes: 2000,
            memory_max: 2000,
            memory_swap_max: 0,
            paused: false,
        })
    }
    fn identify(&self, entry: &Entry, _: i32) -> Result<Identity> {
        Ok(Identity {
            invocation: "0123456789abcdef0123456789abcdef".into(),
            cgroup: format!("/test/{}", entry.unit()),
            inode: 1,
        })
    }
    fn terminated(&self, _: &Entry) -> Option<bool> {
        Some(self.terminated.load(Ordering::SeqCst))
    }
    fn stop(&self, _: &Entry) -> Result<()> {
        Ok(())
    }
}

fn policy() -> Policy {
    Policy {
        version: 1,
        budget_bytes: 1000,
        reserve_bytes: 100,
        queue_limit: 16,
        contracts: BTreeMap::from([(
            "tool".into(),
            Contract {
                slice: "agent-tools.slice".into(),
                memory_max: 600,
                memory_swap_max: 0,
                max_running: 2,
                pause_file: None,
            },
        )]),
    }
}

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for coordinator"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn start(
    root: &Path,
    done: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
) -> thread::JoinHandle<Result<()>> {
    let root = root.to_path_buf();
    thread::spawn(move || {
        serve(
            policy(),
            &root.join("run/admission.sock"),
            &root.join("state"),
            Fake { terminated },
            || done.load(Ordering::SeqCst),
        )
    })
}

fn enqueue(socket: PathBuf) -> Entry {
    call(
        &socket,
        Message::Enqueue {
            contract: "tool".into(),
            wait_ms: 5000,
        },
    )
    .unwrap()
    .entry
    .unwrap()
}

#[test]
fn independent_socket_clients_and_restart_keep_live_reservations() {
    let root = std::env::temp_dir().join(format!("amc-ipc-{}", fresh_id().unwrap()));
    let socket = root.join("run/admission.sock");
    let done = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    let server = start(&root, done.clone(), terminated.clone());
    wait(|| call(&socket, Message::Status).is_ok());
    let a = thread::spawn({
        let socket = socket.clone();
        move || enqueue(socket)
    })
    .join()
    .unwrap();
    let b = thread::spawn({
        let socket = socket.clone();
        move || enqueue(socket)
    })
    .join()
    .unwrap();
    wait(|| {
        call(&socket, Message::Poll { id: a.id.clone() })
            .unwrap()
            .entry
            .unwrap()
            .phase
            == Phase::Reserved
    });
    assert_eq!(
        call(&socket, Message::Poll { id: b.id.clone() })
            .unwrap()
            .entry
            .unwrap()
            .phase,
        Phase::Queued
    );
    call(&socket, Message::Enter { id: a.id.clone() }).unwrap();
    assert!(call(&socket, Message::Enter { id: a.id.clone() }).is_err());
    call(&socket, Message::Cancel { id: a.id.clone() }).unwrap();
    assert_eq!(
        call(&socket, Message::Status)
            .unwrap()
            .status
            .unwrap()
            .committed_bytes,
        600
    );
    done.store(true, Ordering::SeqCst);
    server.join().unwrap().unwrap();
    done.store(false, Ordering::SeqCst);
    let server = start(&root, done.clone(), terminated.clone());
    wait(|| call(&socket, Message::Status).is_ok());
    let status = call(&socket, Message::Status).unwrap().status.unwrap();
    assert_eq!(status.committed_bytes, 600);
    assert_eq!(status.entries.len(), 1);
    assert_eq!(status.entries[0].id, a.id);
    terminated.store(true, Ordering::SeqCst);
    wait(|| {
        call(&socket, Message::Status)
            .unwrap()
            .status
            .unwrap()
            .committed_bytes
            == 0
    });
    done.store(true, Ordering::SeqCst);
    server.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_protocol_never_allocates_capacity() {
    use amc_admission::protocol::{Request, Response, read_frame, write_frame};
    use std::os::unix::net::UnixStream;
    let root = std::env::temp_dir().join(format!("amc-protocol-{}", fresh_id().unwrap()));
    let socket = root.join("run/admission.sock");
    let done = Arc::new(AtomicBool::new(false));
    let server = start(&root, done.clone(), Arc::new(AtomicBool::new(false)));
    wait(|| call(&socket, Message::Status).is_ok());
    let mut stream = UnixStream::connect(&socket).unwrap();
    write_frame(
        &mut stream,
        &Request {
            version: 99,
            message: Message::Enqueue {
                contract: "tool".into(),
                wait_ms: 5000,
            },
        },
    )
    .unwrap();
    assert!(read_frame::<Response>(&mut stream).unwrap().error.is_some());
    assert!(
        call(
            &socket,
            Message::Enqueue {
                contract: "foreign".into(),
                wait_ms: 5000
            }
        )
        .is_err()
    );
    assert!(
        call(&socket, Message::Status)
            .unwrap()
            .status
            .unwrap()
            .entries
            .is_empty()
    );
    done.store(true, Ordering::SeqCst);
    server.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
