use amc_admission::protocol::{read_frame, write_frame};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn trickling_bytes_cannot_extend_the_complete_frame_deadline() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    let timeout = Some(Duration::from_millis(200));
    reader.set_read_timeout(timeout).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let done = stopped.clone();
    let sender = thread::spawn(move || {
        writer.write_all(b"{").unwrap();
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline && !done.load(Ordering::Relaxed) {
            if writer.write_all(b" ").is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = writer.write_all(b"\"version\":1}\n");
    });
    let start = Instant::now();
    let result = read_frame::<Value>(&mut reader);
    let elapsed = start.elapsed();
    stopped.store(true, Ordering::Relaxed);
    sender.join().unwrap();
    assert!(
        result.is_err(),
        "trickling peer completed after its frame deadline"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "frame held the coordinator for {elapsed:?}"
    );
    assert_eq!(reader.read_timeout().unwrap(), timeout);
}

#[test]
fn slowly_drained_responses_have_a_total_write_deadline() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let timeout = Some(Duration::from_millis(200));
    writer.set_write_timeout(timeout).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let done = stopped.clone();
    let receiver = thread::spawn(move || {
        let mut chunk = [0; 4096];
        while !done.load(Ordering::Relaxed) {
            if matches!(reader.read(&mut chunk), Ok(0)) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let start = Instant::now();
    let result = write_frame(&mut writer, &"x".repeat(1_048_576));
    let elapsed = start.elapsed();
    stopped.store(true, Ordering::Relaxed);
    receiver.join().unwrap();
    assert!(
        result.is_err(),
        "slowly drained response exceeded its frame deadline"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "response held the coordinator for {elapsed:?}"
    );
    assert_eq!(writer.write_timeout().unwrap(), timeout);
}

#[test]
fn complete_frames_preserve_the_callers_socket_timeouts() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    let timeout = Some(Duration::from_secs(1));
    reader.set_read_timeout(timeout).unwrap();
    writer.set_write_timeout(timeout).unwrap();
    let value = json!({"version": 1});
    write_frame(&mut writer, &value).unwrap();
    assert_eq!(read_frame::<Value>(&mut reader).unwrap(), value);
    assert_eq!(reader.read_timeout().unwrap(), timeout);
    assert_eq!(writer.write_timeout().unwrap(), timeout);
}
