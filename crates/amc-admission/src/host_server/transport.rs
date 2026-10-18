//! Bounded public-socket framing, independent of the broker's durable ledger.
use super::{Request, Response};
use crate::protocol::MAX_FRAME;
use anyhow::{Result, ensure};
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, poll},
    sys::socket::{MsgFlags, UnixCredentials, getsockopt, recv, send, sockopt::PeerCredentials},
};
use std::{
    os::{
        fd::{AsFd, AsRawFd},
        unix::net::{UnixListener, UnixStream},
    },
    time::{Duration, Instant},
};

const FRAME_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_CONNECTIONS: usize = 32;
const PER_UID_CONNECTIONS: usize = 4;

enum Frame {
    Reading(Vec<u8>),
    Ready(Request),
    Writing { bytes: Vec<u8>, offset: usize },
}

struct Peer {
    stream: UnixStream,
    credentials: UnixCredentials,
    deadline: Instant,
    frame: Frame,
}

impl Peer {
    fn poll(&mut self, progress: &mut bool) -> bool {
        if Instant::now() >= self.deadline {
            *progress = true;
            return false;
        }
        match &mut self.frame {
            Frame::Reading(buffer) => {
                let mut chunk = [0; 8192];
                let bound = (MAX_FRAME + 1 - buffer.len() as u64).min(chunk.len() as u64) as usize;
                match recv(
                    self.stream.as_raw_fd(),
                    &mut chunk[..bound],
                    MsgFlags::MSG_DONTWAIT,
                ) {
                    Ok(0) => {
                        *progress = true;
                        return false;
                    }
                    Ok(count) => {
                        *progress = true;
                        let end = chunk[..count].iter().position(|byte| *byte == b'\n');
                        buffer.extend_from_slice(&chunk[..end.map_or(count, |index| index + 1)]);
                        if buffer.len() as u64 > MAX_FRAME {
                            return false;
                        }
                        if end.is_some() {
                            let Ok(request) = serde_json::from_slice(buffer) else {
                                return false;
                            };
                            if Instant::now() >= self.deadline {
                                return false;
                            }
                            self.frame = Frame::Ready(request);
                        }
                    }
                    Err(Errno::EAGAIN | Errno::EINTR) => {}
                    Err(_) => {
                        *progress = true;
                        return false;
                    }
                }
            }
            Frame::Writing { bytes, offset } => {
                match send(
                    self.stream.as_raw_fd(),
                    &bytes[*offset..],
                    MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
                ) {
                    Ok(0) => {
                        *progress = true;
                        return false;
                    }
                    Ok(count) => {
                        *progress = true;
                        *offset += count;
                    }
                    Err(Errno::EAGAIN | Errno::EINTR) => {}
                    Err(_) => {
                        *progress = true;
                        return false;
                    }
                }
                if *offset == bytes.len() {
                    return false;
                }
            }
            Frame::Ready(_) => {}
        }
        true
    }
}

pub(super) struct Incoming {
    pub stream: UnixStream,
    pub credentials: UnixCredentials,
    pub request: Request,
}

#[derive(Default)]
pub(super) struct Connections {
    peers: Vec<Peer>,
    progress: bool,
}

impl Connections {
    pub fn next(&mut self, listener: &UnixListener) -> Result<Option<Incoming>> {
        self.progress = false;
        self.peers.retain_mut(|peer| peer.poll(&mut self.progress));
        match listener.accept() {
            Ok((stream, _)) => {
                self.progress = true;
                stream.set_read_timeout(Some(FRAME_TIMEOUT))?;
                stream.set_write_timeout(Some(FRAME_TIMEOUT))?;
                let credentials = getsockopt(&stream, PeerCredentials)?;
                let uid = credentials.uid();
                if self
                    .peers
                    .iter()
                    .filter(|peer| peer.credentials.uid() == uid)
                    .count()
                    >= PER_UID_CONNECTIONS
                {
                    // An idle same-UID peer cannot permanently occupy the UID's
                    // slots. Complete requests and replies keep their position.
                    if let Some(index) = self.peers.iter().position(|peer| {
                        peer.credentials.uid() == uid && matches!(peer.frame, Frame::Reading(_))
                    }) {
                        self.peers.remove(index);
                    } else {
                        return Ok(self.take_ready());
                    }
                }
                // Reserve a UID's worth of slots for root recovery/runner RPCs.
                if self.peers.len() >= MAX_CONNECTIONS
                    || (uid != 0
                        && self
                            .peers
                            .iter()
                            .filter(|peer| peer.credentials.uid() != 0)
                            .count()
                            >= MAX_CONNECTIONS - PER_UID_CONNECTIONS)
                {
                    return Ok(self.take_ready());
                }
                let mut peer = Peer {
                    stream,
                    credentials,
                    deadline: Instant::now() + FRAME_TIMEOUT,
                    frame: Frame::Reading(Vec::new()),
                };
                if peer.poll(&mut self.progress) {
                    self.peers.push(peer);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        Ok(self.take_ready())
    }

    fn take_ready(&mut self) -> Option<Incoming> {
        let index = self
            .peers
            .iter()
            .position(|peer| matches!(peer.frame, Frame::Ready(_)))?;
        let peer = self.peers.remove(index);
        let Frame::Ready(request) = peer.frame else {
            unreachable!()
        };
        Some(Incoming {
            stream: peer.stream,
            credentials: peer.credentials,
            request,
        })
    }

    pub fn respond(&mut self, stream: UnixStream, uid: u32, reply: &Response) -> Result<()> {
        // At most one off-loop inventory can complete after its original slot
        // was released. Its reply gets one additional bounded slot.
        ensure!(
            self.peers.len() < MAX_CONNECTIONS + 1,
            "host response queue full"
        );
        let mut bytes = serde_json::to_vec(reply)?;
        ensure!(
            (bytes.len() as u64) < MAX_FRAME,
            "admission response exceeds size bound"
        );
        bytes.push(b'\n');
        let credentials = getsockopt(&stream, PeerCredentials)?;
        ensure!(credentials.uid() == uid, "host response peer changed");
        self.peers.push(Peer {
            stream,
            credentials,
            deadline: Instant::now() + FRAME_TIMEOUT,
            frame: Frame::Writing { bytes, offset: 0 },
        });
        Ok(())
    }

    pub fn progress(&self) -> bool {
        self.progress
    }

    pub fn wait(&self, listener: &UnixListener, inventory: Option<&UnixStream>) -> Result<()> {
        let mut fds = vec![PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        for peer in &self.peers {
            let events = match &peer.frame {
                Frame::Reading(_) => PollFlags::POLLIN,
                Frame::Writing { .. } => PollFlags::POLLOUT,
                Frame::Ready(_) => return Ok(()),
            };
            fds.push(PollFd::new(peer.stream.as_fd(), events));
        }
        if let Some(inventory) = inventory {
            fds.push(PollFd::new(inventory.as_fd(), PollFlags::POLLIN));
        }
        // Keep the existing idle bound for stop/manager/deadline checks, but
        // wake immediately for ready requests, replies or worker completion.
        match poll(&mut fds, 20u16) {
            Ok(_) | Err(Errno::EINTR) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{read_frame, write_frame};
    use std::{io::Write, thread, time::Instant};

    #[test]
    fn incomplete_public_frames_do_not_delay_a_complete_status_request() {
        let directory =
            std::env::temp_dir().join(format!("amc-frames-{}", crate::store::fresh_id().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut incomplete = Vec::new();
        for _ in 0..12 {
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream.write_all(b"{\"op\":").unwrap();
            incomplete.push(stream);
        }
        let client = UnixStream::connect(&socket).unwrap();
        let worker = thread::spawn(move || {
            let mut connections = Connections::default();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Some(incoming) = connections.next(&listener).unwrap() {
                    assert!(matches!(incoming.request, Request::Status { version: 1 }));
                    let _ = connections.respond(
                        incoming.stream,
                        incoming.credentials.uid(),
                        &Response {
                            version: 1,
                            committed_bytes: 17,
                            ..Default::default()
                        },
                    );
                    while connections.progress() && Instant::now() < deadline {
                        connections.next(&listener).unwrap();
                    }
                    return;
                }
                thread::yield_now();
            }
            panic!("complete request never reached the broker");
        });
        let began = Instant::now();
        let reply = super::super::exchange(client, &Request::Status { version: 1 });
        let elapsed = began.elapsed();
        worker.join().unwrap();
        drop(incomplete);
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(
            reply.is_ok(),
            "incomplete peers exhausted the ordinary RPC deadline: {reply:?}"
        );
        assert_eq!(reply.unwrap().committed_bytes, 17);
        assert!(
            elapsed < Duration::from_millis(500),
            "status waited {elapsed:?} behind incomplete frames"
        );
    }

    #[test]
    fn unread_public_replies_do_not_block_the_broker() {
        let (_client, stream) = UnixStream::pair().unwrap();
        stream
            .set_write_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let mut connections = Connections::default();
        let began = Instant::now();
        connections
            .respond(
                stream,
                nix::unistd::geteuid().as_raw(),
                &Response {
                    version: 1,
                    error: Some("x".repeat(1024 * 1024)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "an unread response occupied the broker loop"
        );

        let directory =
            std::env::temp_dir().join(format!("amc-replies-{}", crate::store::fresh_id().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        // Fill the unread peer's kernel buffer while a real status RPC enters.
        for _ in 0..64 {
            connections.next(&listener).unwrap();
        }
        let mut client = UnixStream::connect(&socket).unwrap();
        write_frame(&mut client, &Request::Status { version: 1 }).unwrap();
        let began = Instant::now();
        let incoming = connections
            .next(&listener)
            .unwrap()
            .expect("status frame stays serviceable beside an unread reply");
        assert!(matches!(incoming.request, Request::Status { version: 1 }));
        connections
            .respond(
                incoming.stream,
                incoming.credentials.uid(),
                &Response {
                    version: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        connections.next(&listener).unwrap();
        assert!(read_frame::<Response>(&mut client).unwrap().error.is_none());
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "unread peer delayed another RPC"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn fragmented_requests_survive_across_nonblocking_broker_turns() {
        let directory = std::env::temp_dir().join(format!(
            "amc-fragments-{}",
            crate::store::fresh_id().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = UnixStream::connect(&socket).unwrap();
        let mut connections = Connections::default();
        client.write_all(b"{\"op\":\"status\",").unwrap();
        assert!(connections.next(&listener).unwrap().is_none());
        assert!(connections.next(&listener).unwrap().is_none());
        client.write_all(b"\"version\":1}\n").unwrap();
        let incoming = connections
            .next(&listener)
            .unwrap()
            .expect("complete fragmented frame");
        assert!(matches!(incoming.request, Request::Status { version: 1 }));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn ready_host_work_does_not_pay_the_idle_interval_for_each_turn() {
        let directory = std::env::temp_dir().join(format!(
            "amc-ready-host-{}",
            crate::store::fresh_id().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut connections = Connections::default();
        let began = Instant::now();
        for _ in 0..16 {
            let mut client = UnixStream::connect(&socket).unwrap();
            connections.wait(&listener, None).unwrap();
            assert!(connections.next(&listener).unwrap().is_none());
            client
                .write_all(b"{\"op\":\"status\",\"version\":1}\n")
                .unwrap();
            connections.wait(&listener, None).unwrap();
            let incoming = connections.next(&listener).unwrap().unwrap();
            assert!(matches!(incoming.request, Request::Status { version: 1 }));
            connections
                .respond(
                    incoming.stream,
                    incoming.credentials.uid(),
                    &Response {
                        version: 1,
                        committed_bytes: 17,
                        ..Default::default()
                    },
                )
                .unwrap();
            connections.wait(&listener, None).unwrap();
            assert!(connections.next(&listener).unwrap().is_none());
            assert_eq!(
                read_frame::<Response>(&mut client).unwrap().committed_bytes,
                17
            );

            let (completion, worker) = UnixStream::pair().unwrap();
            drop(worker);
            connections.wait(&listener, Some(&completion)).unwrap();
        }
        let elapsed = began.elapsed();
        std::fs::remove_dir_all(directory).unwrap();
        eprintln!("64 ready broker waits: {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(500),
            "ready listener/frame/reply/inventory work paid idle sleeps: {elapsed:?}"
        );
    }
}
