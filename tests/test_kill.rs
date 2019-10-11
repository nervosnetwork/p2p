#![cfg(target_os = "linux")]
use bytes::Bytes;
use futures::prelude::Stream;
use nix::{
    sys::signal::{kill, Signal},
    unistd::{fork, ForkResult},
};
use std::{thread, time::Duration};
use systemstat::{Platform, System};
use tentacle::{
    builder::{MetaBuilder, ServiceBuilder},
    context::{ProtocolContext, ProtocolContextMutRef},
    secio::SecioKeyPair,
    service::{DialProtocol, ProtocolHandle, ProtocolMeta, Service, TargetSession},
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId,
};

/// Get current used memory(bytes)
fn current_used_memory() -> Option<f64> {
    let sys = System::new();
    match sys.memory() {
        Ok(mem) => Some((mem.total.as_u64() - mem.free.as_u64()) as f64),
        Err(_) => None,
    }
}

/// Get current used cpu(all cores) average usage ratio
fn current_used_cpu() -> Option<f32> {
    let sys = System::new();
    match sys.cpu_load_aggregate() {
        Ok(cpu) => {
            thread::sleep(Duration::from_secs(1));
            cpu.done().ok().map(|cpu| cpu.user)
        }
        Err(_) => None,
    }
}

pub fn create<F>(secio: bool, meta: ProtocolMeta, shandle: F) -> Service<F>
where
    F: ServiceHandle,
{
    let builder = ServiceBuilder::default()
        .insert_protocol(meta)
        .forever(true);

    if secio {
        builder
            .key_pair(SecioKeyPair::secp256k1_generated())
            .build(shandle)
    } else {
        builder.build(shandle)
    }
}

struct PHandle {
    connected_count: usize,
    sender: crossbeam_channel::Sender<()>,
}

impl ServiceProtocol for PHandle {
    fn init(&mut self, _context: &mut ProtocolContext) {}

    fn connected(&mut self, _context: ProtocolContextMutRef, _version: &str) {
        self.connected_count += 1;
        assert_eq!(self.sender.send(()), Ok(()));
    }

    fn disconnected(&mut self, _context: ProtocolContextMutRef) {
        self.connected_count -= 1;
        assert_eq!(self.sender.send(()), Ok(()));
    }

    fn received(&mut self, context: ProtocolContextMutRef, data: bytes::Bytes) {
        let proto_id = context.proto_id;
        let _ = context.filter_broadcast(TargetSession::All, proto_id, data);
    }
}

fn create_meta(id: ProtocolId) -> (ProtocolMeta, crossbeam_channel::Receiver<()>) {
    let (sender, receiver) = crossbeam_channel::bounded(1);

    let meta = MetaBuilder::new()
        .id(id)
        .service_handle(move || {
            if id == 0.into() {
                ProtocolHandle::Neither
            } else {
                let handle = Box::new(PHandle {
                    connected_count: 0,
                    sender,
                });
                ProtocolHandle::Callback(handle)
            }
        })
        .build();

    (meta, receiver)
}

/// Test just like https://github.com/libp2p/rust-libp2p/issues/648 this issue, kill some peer
/// and observe if there has a memory leak, cpu takes up too much problem
fn test_kill(secio: bool) {
    let (meta, receiver) = create_meta(1.into());
    let mut service = create(secio, meta, ());
    let listen_addr = service
        .listen("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .unwrap();
    let control = service.control().clone();
    thread::spawn(|| tokio::run(service.for_each(|_| Ok(()))));
    thread::sleep(Duration::from_millis(100));

    match fork() {
        Err(e) => panic!("Fork failed, {}", e),
        Ok(ForkResult::Parent { child }) => {
            // wait connected
            assert_eq!(receiver.recv(), Ok(()));

            let _ =
                control.filter_broadcast(TargetSession::All, 1.into(), Bytes::from("hello world"));
            let mem_start = current_used_memory().unwrap();
            let cpu_start = current_used_cpu().unwrap();

            thread::sleep(Duration::from_secs(10));
            assert_eq!(kill(child, Signal::SIGKILL), Ok(()));
            assert_eq!(receiver.recv(), Ok(()));

            let mem_stop = current_used_memory().unwrap();
            let cpu_stop = current_used_cpu().unwrap();
            assert!((mem_stop - mem_start) / mem_start < 0.1);
            assert!((cpu_stop - cpu_start) / cpu_start < 0.1);
        }
        Ok(ForkResult::Child) => {
            let (meta, _receiver) = create_meta(1.into());
            let mut service = create(secio, meta, ());
            service.dial(listen_addr, DialProtocol::All).unwrap();
            let handle = thread::spawn(|| tokio::run(service.for_each(|_| Ok(()))));
            handle.join().expect("child process done")
        }
    }
}

#[test]
fn test_kill_with_secio() {
    test_kill(true)
}

#[test]
fn test_kill_with_no_secio() {
    test_kill(false)
}
