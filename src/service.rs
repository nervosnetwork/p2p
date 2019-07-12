use futures::{prelude::*, sync::mpsc};
use log::{debug, error, trace, warn};
use std::collections::{vec_deque::VecDeque, HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use std::{error::Error as ErrorTrait, io};
use tokio::prelude::{AsyncRead, AsyncWrite, FutureExt};
use tokio::timer::{Delay, Interval};

use crate::{
    context::{ServiceContext, SessionContext, SessionController},
    error::Error,
    multiaddr::{multihash::Multihash, Multiaddr, Protocol},
    protocol_handle_stream::{
        ServiceProtocolEvent, ServiceProtocolStream, SessionProtocolEvent, SessionProtocolStream,
    },
    protocol_select::ProtocolInfo,
    secio::{handshake::Config, PublicKey, SecioKeyPair},
    service::{
        config::{ServiceConfig, State},
        event::{Priority, ServiceTask},
        future_task::{BlockingFutureTask, BoxedFutureTask, FutureTaskManager},
    },
    session::{Session, SessionEvent, SessionMeta},
    traits::{ServiceHandle, ServiceProtocol, SessionProtocol},
    transports::{MultiIncoming, MultiTransport, Transport, TransportError},
    upnp::IGDClient,
    utils::extract_peer_id,
    yamux::{session::SessionType as YamuxType, Config as YamuxConfig},
    ProtocolId, SessionId,
};

pub(crate) mod config;
mod control;
pub(crate) mod event;
pub(crate) mod future_task;

pub use crate::service::{
    config::{DialProtocol, ProtocolHandle, ProtocolMeta, TargetProtocol, TargetSession},
    control::ServiceControl,
    event::{ProtocolEvent, ServiceError, ServiceEvent},
};
use bytes::Bytes;

/// If the buffer capacity is greater than u8 max, shrink it
pub(crate) const BUF_SHRINK_THRESHOLD: usize = u8::max_value() as usize;
/// Received from user, aggregate mode
pub(crate) const RECEIVED_BUFFER_SIZE: usize = 2048;
/// Use to receive open/close event, no need too large
pub(crate) const RECEIVED_SIZE: usize = 512;
/// Send to remote, distribute mode
pub(crate) const SEND_SIZE: usize = 512;
pub(crate) const DELAY_TIME: Duration = Duration::from_millis(300);

/// Protocol handle value
pub(crate) enum InnerProtocolHandle {
    /// Service level protocol
    Service(Box<dyn ServiceProtocol + Send + 'static>),
    /// Session level protocol
    Session(Box<dyn SessionProtocol + Send + 'static>),
}

/// An abstraction of p2p service, currently only supports TCP protocol
pub struct Service<T> {
    protocol_configs: HashMap<String, ProtocolMeta>,

    sessions: HashMap<SessionId, SessionController>,

    multi_transport: MultiTransport,

    listens: Vec<(Multiaddr, MultiIncoming)>,

    igd_client: Option<IGDClient>,

    dial_protocols: HashMap<Multiaddr, TargetProtocol>,
    config: ServiceConfig,
    /// service state
    state: State,

    next_session: SessionId,

    before_sends: HashMap<ProtocolId, Box<dyn Fn(bytes::Bytes) -> bytes::Bytes + Send + 'static>>,

    /// Can be upgrade to list service level protocols
    handle: T,
    /// The buffer will be prioritized for distribution to session
    high_write_buf: VecDeque<(SessionId, SessionEvent)>,
    /// The buffer which will distribute to sessions
    write_buf: VecDeque<(SessionId, SessionEvent)>,
    /// The buffer which will distribute to service protocol handle
    read_service_buf: VecDeque<(Option<SessionId>, ProtocolId, ServiceProtocolEvent)>,
    /// The buffer which will distribute to session protocol handle
    read_session_buf: VecDeque<(SessionId, ProtocolId, SessionProtocolEvent)>,

    // Future task manager
    future_task_manager: Option<FutureTaskManager>,
    // To add a future task
    // TODO: use this to spawn every task
    future_task_sender: mpsc::Sender<BoxedFutureTask>,

    // The service protocols open with the session
    session_service_protos: HashMap<SessionId, HashSet<ProtocolId>>,

    service_proto_handles: HashMap<ProtocolId, mpsc::Sender<ServiceProtocolEvent>>,

    session_proto_handles: HashMap<(SessionId, ProtocolId), mpsc::Sender<SessionProtocolEvent>>,

    /// Send events to service, clone to session
    session_event_sender: mpsc::Sender<SessionEvent>,
    /// Receive event from service
    session_event_receiver: mpsc::Receiver<SessionEvent>,

    /// External event is passed in from this
    service_context: ServiceContext,
    /// External event receiver
    service_task_receiver: mpsc::UnboundedReceiver<ServiceTask>,
    quick_task_receiver: mpsc::UnboundedReceiver<ServiceTask>,

    pending_tasks: VecDeque<BoxedFutureTask>,
    /// Delay notify with abnormally poor machines
    delay: Arc<AtomicBool>,

    shutdown: Arc<AtomicBool>,
}

impl<T> Service<T>
where
    T: ServiceHandle,
{
    /// New a Service
    pub(crate) fn new(
        protocol_configs: HashMap<String, ProtocolMeta>,
        handle: T,
        key_pair: Option<SecioKeyPair>,
        forever: bool,
        config: ServiceConfig,
    ) -> Self {
        let (session_event_sender, session_event_receiver) = mpsc::channel(RECEIVED_SIZE);
        let (service_task_sender, service_task_receiver) = mpsc::unbounded();
        let (quick_task_sender, quick_task_receiver) = mpsc::unbounded();
        let proto_infos = protocol_configs
            .values()
            .map(|meta| {
                let proto_info = ProtocolInfo::new(&meta.name(), meta.support_versions());
                (meta.id(), proto_info)
            })
            .collect();
        let (future_task_sender, future_task_receiver) = mpsc::channel(SEND_SIZE);
        let shutdown = Arc::new(AtomicBool::new(false));
        let igd_client = if config.upnp { IGDClient::new() } else { None };

        Service {
            protocol_configs,
            before_sends: HashMap::default(),
            handle,
            multi_transport: MultiTransport::new(config.timeout),
            future_task_sender,
            future_task_manager: Some(FutureTaskManager::new(
                future_task_receiver,
                shutdown.clone(),
            )),
            sessions: HashMap::default(),
            session_service_protos: HashMap::default(),
            service_proto_handles: HashMap::default(),
            session_proto_handles: HashMap::default(),
            listens: Vec::new(),
            igd_client,
            dial_protocols: HashMap::default(),
            state: State::new(forever),
            next_session: SessionId::default(),
            high_write_buf: VecDeque::default(),
            write_buf: VecDeque::default(),
            read_service_buf: VecDeque::default(),
            read_session_buf: VecDeque::default(),
            session_event_sender,
            session_event_receiver,
            service_context: ServiceContext::new(
                service_task_sender,
                quick_task_sender,
                proto_infos,
                key_pair,
                shutdown.clone(),
                config.timeout,
            ),
            config,
            service_task_receiver,
            quick_task_receiver,
            pending_tasks: VecDeque::default(),
            delay: Arc::new(AtomicBool::new(false)),
            shutdown,
        }
    }

    /// Yamux config for service
    ///
    /// Panic when max_frame_length < yamux_max_window_size
    pub fn yamux_config(mut self, config: YamuxConfig) -> Self {
        assert!(self.config.max_frame_length as u32 >= config.max_stream_window_size);
        self.config.yamux_config = config;
        self
    }

    /// Secio max frame length
    ///
    /// Panic when max_frame_length < yamux_max_window_size
    pub fn max_frame_length(mut self, size: usize) -> Self {
        assert!(size as u32 >= self.config.yamux_config.max_stream_window_size);
        self.config.max_frame_length = size;
        self
    }

    /// Listen on the given address.
    ///
    /// Return really listen multiaddr, but if use `/dns4/localhost/tcp/80`,
    /// it will return original value, and create a future task to DNS resolver later.
    pub fn listen(&mut self, address: Multiaddr) -> Result<Multiaddr, io::Error> {
        let (listen_future, listen_addr) = self
            .multi_transport
            .listen(address.clone())
            .map_err::<io::Error, _>(Into::into)?;
        let sender = self.session_event_sender.clone();
        let task = listen_future.then(move |result| match result {
            Ok(value) => tokio::spawn(
                sender
                    .send(SessionEvent::ListenStart {
                        listen_address: value.0,
                        incoming: value.1,
                    })
                    .map(|_| ())
                    .map_err(|err| {
                        error!("Listen address success send back error: {:?}", err);
                    }),
            ),
            Err(err) => {
                let event = if let TransportError::DNSResolverError((address, error)) = err {
                    SessionEvent::ListenError {
                        address,
                        error: Error::DNSResolverError(error),
                    }
                } else {
                    SessionEvent::ListenError {
                        address,
                        error: Error::DNSResolverError(io::ErrorKind::InvalidData.into()),
                    }
                };
                tokio::spawn(sender.send(event).map(|_| ()).map_err(|err| {
                    error!("Listen address fail send back error: {:?}", err);
                }))
            }
        });
        self.pending_tasks.push_back(Box::new(task));
        self.state.increase();
        Ok(listen_addr)
    }

    /// Dial the given address, doesn't actually make a request, just generate a future
    pub fn dial(
        &mut self,
        address: Multiaddr,
        target: DialProtocol,
    ) -> Result<&mut Self, io::Error> {
        self.dial_inner(address, target.into())?;
        Ok(self)
    }

    /// Use by inner
    #[inline(always)]
    fn dial_inner(&mut self, address: Multiaddr, target: TargetProtocol) -> Result<(), io::Error> {
        self.dial_protocols.insert(address.clone(), target);
        let dial_future = self
            .multi_transport
            .dial(address.clone())
            .map_err::<io::Error, _>(Into::into)?;

        let sender = self.session_event_sender.clone();
        let task = dial_future.then(|result| match result {
            Ok(value) => tokio::spawn(
                sender
                    .send(SessionEvent::DialStart {
                        remote_address: value.0,
                        stream: value.1,
                    })
                    .map(|_| ())
                    .map_err(|err| {
                        error!("dial address success send back error: {:?}", err);
                    }),
            ),
            Err(err) => {
                let event = match err {
                    TransportError::DNSResolverError((address, error)) => SessionEvent::DialError {
                        address,
                        error: Error::DNSResolverError(error),
                    },
                    e => SessionEvent::DialError {
                        address,
                        error: Error::IoError(e.into()),
                    },
                };
                tokio::spawn(sender.send(event).map(|_| ()).map_err(|err| {
                    error!("dial address fail send back error: {:?}", err);
                }))
            }
        });

        self.pending_tasks.push_back(Box::new(task));
        self.state.increase();
        Ok(())
    }

    /// Get service current protocol configure
    pub fn protocol_configs(&self) -> &HashMap<String, ProtocolMeta> {
        &self.protocol_configs
    }

    /// Get service control, control can send tasks externally to the runtime inside
    pub fn control(&self) -> &ServiceControl {
        self.service_context.control()
    }

    fn push_back(&mut self, priority: Priority, id: SessionId, event: SessionEvent) {
        if priority.is_high() {
            self.high_write_buf.push_back((id, event));
        } else {
            self.write_buf.push_back((id, event));
        }
    }

    #[inline(always)]
    fn distribute_to_session_process<D: Iterator<Item = (SessionId, SessionEvent)>>(
        &mut self,
        data: D,
        priority: Priority,
        block_sessions: &mut HashSet<SessionId>,
    ) {
        for (id, event) in data {
            // Guarantee the order in which messages are sent
            if block_sessions.contains(&id) {
                self.push_back(priority, id, event);
                continue;
            }
            if let Some(session) = self.sessions.get_mut(&id) {
                if session.inner.closed.load(Ordering::SeqCst) {
                    if let SessionEvent::ProtocolMessage { .. } = event {
                        continue;
                    }
                }

                if let Err(e) = session.try_send(priority, event) {
                    if e.is_full() {
                        block_sessions.insert(id);
                        debug!("session [{}] is full", id);
                        self.push_back(priority, id, e.into_inner());
                        self.set_delay();
                    } else {
                        debug!(
                            "session {} has been shutdown, message can't send, just drop it",
                            id
                        )
                    }
                }
            } else {
                debug!("Can't find session {} to send data", id);
            }
        }
    }

    /// Distribute event to sessions
    #[inline]
    fn distribute_to_session(&mut self) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let mut block_sessions = HashSet::new();

        let high = self.high_write_buf.split_off(0).into_iter();
        self.distribute_to_session_process(high, Priority::High, &mut block_sessions);

        if self.sessions.len() > block_sessions.len() {
            let normal = self.write_buf.split_off(0).into_iter();
            self.distribute_to_session_process(normal, Priority::Normal, &mut block_sessions);
        }

        for id in block_sessions {
            if let Some(control) = self.sessions.get(&id) {
                self.handle.handle_error(
                    &mut self.service_context,
                    ServiceError::SessionBlocked {
                        session_context: control.inner.clone(),
                    },
                );
            }
        }

        if self.write_buf.capacity() > BUF_SHRINK_THRESHOLD {
            self.write_buf.shrink_to_fit();
        }

        if self.high_write_buf.capacity() > BUF_SHRINK_THRESHOLD {
            self.high_write_buf.shrink_to_fit();
        }
    }

    /// Distribute event to user level
    #[inline(always)]
    fn distribute_to_user_level(&mut self) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let mut error = false;
        let mut block_handles = HashSet::new();

        let closed_sessions = self
            .sessions
            .iter()
            .filter(|(_, control)| control.inner.closed.load(Ordering::SeqCst))
            .map(|(session_id, _)| *session_id)
            .collect::<HashSet<_>>();
        for id in closed_sessions {
            self.session_close(id, Source::Internal);
        }

        for (session_id, proto_id, event) in self.read_service_buf.split_off(0) {
            // Guarantee the order in which messages are sent
            if block_handles.contains(&proto_id) {
                self.read_service_buf
                    .push_back((session_id, proto_id, event));
                continue;
            }
            if let Some(sender) = self.service_proto_handles.get_mut(&proto_id) {
                if let Err(e) = sender.try_send(event) {
                    if e.is_full() {
                        debug!("service proto [{}] handle is full", proto_id);
                        self.read_service_buf
                            .push_back((session_id, proto_id, e.into_inner()));
                        self.proto_handle_error(proto_id, None);
                        block_handles.insert(proto_id);
                    } else {
                        debug!(
                            "channel shutdown, service proto [{}] message can't send to user",
                            proto_id
                        );

                        error = true;
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::ProtocolHandleError {
                                proto_id,
                                error: Error::ServiceProtoHandleAbnormallyClosed,
                            },
                        );
                    }
                }
            }
        }

        for (session_id, proto_id, event) in self.read_session_buf.split_off(0) {
            if let Some(sender) = self.session_proto_handles.get_mut(&(session_id, proto_id)) {
                if let Err(e) = sender.try_send(event) {
                    if e.is_full() {
                        debug!(
                            "session [{}] proto [{}] handle is full",
                            session_id, proto_id
                        );
                        self.read_session_buf
                            .push_back((session_id, proto_id, e.into_inner()));
                        self.proto_handle_error(proto_id, Some(session_id));
                    } else {
                        debug!(
                            "channel shutdown, session proto [{}] session [{}] message can't send to user",
                            proto_id, session_id
                        );

                        error = true;
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::ProtocolHandleError {
                                proto_id,
                                error: Error::SessionProtoHandleAbnormallyClosed(session_id),
                            },
                        )
                    }
                }
            }
        }

        if error {
            // if handle panic, close service
            self.handle_service_task(ServiceTask::Shutdown(false));
        }

        if self.read_service_buf.capacity() > BUF_SHRINK_THRESHOLD {
            self.read_service_buf.shrink_to_fit();
        }

        if self.read_session_buf.capacity() > BUF_SHRINK_THRESHOLD {
            self.read_session_buf.shrink_to_fit();
        }
    }

    /// When proto handle channel is full, call here
    #[inline]
    fn proto_handle_error(&mut self, proto_id: ProtocolId, session_id: Option<SessionId>) {
        let error = session_id
            .map(Error::SessionProtoHandleBlock)
            .unwrap_or(Error::ServiceProtoHandleBlock);
        self.set_delay();
        self.handle.handle_error(
            &mut self.service_context,
            ServiceError::ProtocolHandleError { proto_id, error },
        );
    }

    /// Spawn protocol handle
    #[inline]
    fn handle_open(
        &mut self,
        handle: InnerProtocolHandle,
        proto_id: ProtocolId,
        id: Option<SessionId>,
    ) {
        match handle {
            InnerProtocolHandle::Service(handle) => {
                debug!("init service level [{}] proto handle", proto_id);
                let (sender, receiver) = mpsc::channel(RECEIVED_SIZE);
                self.service_proto_handles.insert(proto_id, sender);

                let mut stream = ServiceProtocolStream::new(
                    handle,
                    self.service_context.clone_self(),
                    receiver,
                    proto_id,
                    (self.shutdown.clone(), self.future_task_sender.clone()),
                );
                stream.handle_event();
                tokio::spawn(stream.for_each(|_| Ok(())).map_err(|_| ()));
            }

            InnerProtocolHandle::Session(handle) => {
                let id = id.unwrap();
                if let Some(session_control) = self.sessions.get(&id) {
                    debug!("init session [{}] level proto [{}] handle", id, proto_id);
                    let (sender, receiver) = mpsc::channel(RECEIVED_SIZE);
                    self.session_proto_handles.insert((id, proto_id), sender);

                    let stream = SessionProtocolStream::new(
                        handle,
                        self.service_context.clone_self(),
                        Arc::clone(&session_control.inner),
                        receiver,
                        proto_id,
                        (self.shutdown.clone(), self.future_task_sender.clone()),
                    );
                    tokio::spawn(stream.for_each(|_| Ok(())).map_err(|_| ()));
                }
            }
        }
    }

    /// Send data to the specified protocol for the specified session.
    #[inline]
    fn send_message_to(
        &mut self,
        session_id: SessionId,
        proto_id: ProtocolId,
        priority: Priority,
        data: Bytes,
    ) {
        if !self.sessions.contains_key(&session_id) {
            return;
        }
        let message_event = SessionEvent::ProtocolMessage {
            id: session_id,
            proto_id,
            priority,
            data,
        };
        self.push_back(priority, session_id, message_event);

        self.distribute_to_session();
    }

    /// Send data to the specified protocol for the specified sessions.
    #[inline]
    fn filter_broadcast(
        &mut self,
        ids: Vec<SessionId>,
        proto_id: ProtocolId,
        priority: Priority,
        data: Bytes,
    ) {
        for id in self.sessions.keys().cloned().collect::<Vec<SessionId>>() {
            if ids.contains(&id) {
                debug!(
                    "send message to session [{}], proto [{}], data len: {}",
                    id,
                    proto_id,
                    data.len()
                );

                let message_event = SessionEvent::ProtocolMessage {
                    id,
                    proto_id,
                    priority,
                    data: data.clone(),
                };
                self.push_back(priority, id, message_event);
            }
        }
        self.distribute_to_session();
    }

    /// Broadcast data for a specified protocol.
    #[inline]
    fn broadcast(&mut self, proto_id: ProtocolId, priority: Priority, data: Bytes) {
        debug!(
            "broadcast message, peer count: {}, proto_id: {}, data len: {}",
            self.sessions.len(),
            proto_id,
            data.len()
        );
        for id in self.sessions.keys().cloned().collect::<Vec<SessionId>>() {
            let message_event = SessionEvent::ProtocolMessage {
                id,
                proto_id,
                priority,
                data: data.clone(),
            };
            self.push_back(priority, id, message_event);
        }
        self.distribute_to_session();
    }

    /// Get the callback handle of the specified protocol
    #[inline]
    fn proto_handle(&mut self, session: bool, proto_id: ProtocolId) -> Option<InnerProtocolHandle> {
        let handle = self.protocol_configs.values_mut().find_map(|proto| {
            if proto.id() == proto_id {
                if session {
                    match proto.session_handle() {
                        ProtocolHandle::Callback(handle) | ProtocolHandle::Both(handle) => {
                            Some(InnerProtocolHandle::Session(handle))
                        }
                        _ => None,
                    }
                } else {
                    match proto.service_handle() {
                        ProtocolHandle::Callback(handle) | ProtocolHandle::Both(handle) => {
                            Some(InnerProtocolHandle::Service(handle))
                        }
                        _ => None,
                    }
                }
            } else {
                None
            }
        });

        if handle.is_none() {
            debug!(
                "can't find proto [{}] {} handle",
                proto_id,
                if session { "session" } else { "service" }
            );
        }

        handle
    }

    /// Handshake
    #[inline]
    fn handshake<H>(&mut self, socket: H, ty: SessionType, remote_address: Multiaddr)
    where
        H: AsyncRead + AsyncWrite + Send + 'static,
    {
        if let Some(key_pair) = self.service_context.key_pair() {
            let key_pair = key_pair.clone();
            let sender = self.session_event_sender.clone();

            let handshake_task = Config::new(key_pair)
                .max_frame_length(self.config.max_frame_length)
                .handshake(socket)
                .timeout(self.config.timeout)
                .then(move |result| {
                    let send_task = match result {
                        Ok((handle, public_key, _)) => {
                            sender.send(SessionEvent::HandshakeSuccess {
                                handle,
                                public_key,
                                address: remote_address,
                                ty,
                            })
                        }
                        Err(err) => {
                            let error = if err.is_timer() {
                                // tokio timer error
                                io::Error::new(io::ErrorKind::Other, err.description()).into()
                            } else if err.is_elapsed() {
                                // time out error
                                io::Error::new(io::ErrorKind::TimedOut, err.description()).into()
                            } else {
                                // dialer error
                                err.into_inner().unwrap().into()
                            };

                            debug!(
                                "Handshake with {} failed, error: {:?}",
                                remote_address, error
                            );

                            sender.send(SessionEvent::HandshakeFail {
                                ty,
                                error,
                                address: remote_address,
                            })
                        }
                    };

                    tokio::spawn(send_task.map(|_| ()).map_err(|err| {
                        error!("handshake result send back error: {:?}", err);
                    }));

                    Ok(())
                });

            let future_task = self
                .future_task_sender
                .clone()
                .send(Box::new(handshake_task))
                .map(|_| ())
                .map_err(|_| ());

            tokio::spawn(future_task);
        } else {
            self.session_open(socket, None, remote_address, ty);
        }
    }

    /// Session open
    #[inline]
    fn session_open<H>(
        &mut self,
        mut handle: H,
        remote_pubkey: Option<PublicKey>,
        mut address: Multiaddr,
        ty: SessionType,
    ) where
        H: AsyncRead + AsyncWrite + Send + 'static,
    {
        if ty.is_outbound() {
            self.state.decrease();
        }
        let target = self
            .dial_protocols
            .remove(&address)
            .unwrap_or_else(|| TargetProtocol::All);
        if let Some(ref key) = remote_pubkey {
            // If the public key exists, the connection has been established
            // and then the useless connection needs to be closed.
            match self
                .sessions
                .values()
                .find(|&context| context.inner.remote_pubkey.as_ref() == Some(key))
            {
                Some(context) => {
                    trace!("Connected to the connected node");
                    let _ = handle.shutdown();
                    if ty.is_outbound() {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::DialerError {
                                error: Error::RepeatedConnection(context.inner.id),
                                address,
                            },
                        );
                    } else {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::ListenError {
                                error: Error::RepeatedConnection(context.inner.id),
                                address,
                            },
                        );
                    }
                    return;
                }
                None => {
                    // if peer id doesn't match return an error
                    if let Some(peer_id) = extract_peer_id(&address) {
                        if key.peer_id() != peer_id {
                            trace!("Peer id not match");
                            self.handle.handle_error(
                                &mut self.service_context,
                                ServiceError::DialerError {
                                    error: Error::PeerIdNotMatch,
                                    address,
                                },
                            );
                            return;
                        }
                    } else {
                        address.push(Protocol::P2p(
                            Multihash::from_bytes(key.peer_id().into_bytes())
                                .expect("Invalid peer id"),
                        ))
                    }

                    self.next_session += 1
                }
            }
        } else {
            self.next_session += 1;
        }

        let session_closed = Arc::new(AtomicBool::new(false));
        let (service_event_sender, service_event_receiver) = mpsc::channel(SEND_SIZE);
        let (quick_event_sender, quick_event_receiver) = mpsc::channel(SEND_SIZE);
        let session_control = SessionController::new(
            quick_event_sender,
            service_event_sender,
            Arc::new(SessionContext {
                id: self.next_session,
                address,
                ty,
                remote_pubkey,
                closed: session_closed.clone(),
            }),
        );

        let session_context = session_control.inner.clone();

        // must insert here, otherwise, the session protocol handle cannot be opened
        self.sessions
            .insert(session_control.inner.id, session_control);

        // Open all session protocol handles
        let proto_ids = self
            .protocol_configs
            .values()
            .map(ProtocolMeta::id)
            .collect::<Vec<ProtocolId>>();

        for proto_id in proto_ids {
            if let Some(handle) = self.proto_handle(true, proto_id) {
                self.handle_open(handle, proto_id, Some(self.next_session))
            }
        }

        let meta = SessionMeta::new(self.config.timeout, session_context.clone())
            .protocol(
                self.protocol_configs
                    .iter()
                    .map(|(key, value)| (key.clone(), value.inner.clone()))
                    .collect(),
            )
            .config(self.config.yamux_config)
            .keep_buffer(self.config.keep_buffer)
            .service_proto_senders(self.service_proto_handles.clone())
            .session_senders(
                self.session_proto_handles
                    .iter()
                    .filter_map(|((session_id, key), value)| {
                        if *session_id == self.next_session {
                            Some((*key, value.clone()))
                        } else {
                            None
                        }
                    })
                    .collect(),
            )
            .context(session_context.clone())
            .event(self.config.event.clone());

        let mut session = Session::new(
            handle,
            self.session_event_sender.clone(),
            service_event_receiver,
            quick_event_receiver,
            meta,
            self.future_task_sender.clone(),
        );

        if ty.is_outbound() {
            match target {
                TargetProtocol::All => {
                    self.protocol_configs
                        .keys()
                        .for_each(|name| session.open_proto_stream(name));
                }
                TargetProtocol::Single(proto_id) => {
                    self.protocol_configs
                        .values()
                        .find(|meta| meta.id() == proto_id)
                        .and_then(|meta| {
                            session.open_proto_stream(&meta.name());
                            Some(())
                        });
                }
                TargetProtocol::Multi(proto_ids) => self
                    .protocol_configs
                    .values()
                    .filter(|meta| proto_ids.contains(&meta.id()))
                    .for_each(|meta| session.open_proto_stream(&meta.name())),
            }
        }

        tokio::spawn(session.for_each(|_| Ok(())).map_err(|_| ()));

        self.handle.handle_event(
            &mut self.service_context,
            ServiceEvent::SessionOpen {
                session_context: Arc::clone(&session_context),
            },
        );
    }

    /// Close the specified session, clean up the handle
    #[inline]
    fn session_close(&mut self, id: SessionId, source: Source) {
        if source == Source::External {
            debug!("try close service session [{}] ", id);
            self.write_buf
                .push_back((id, SessionEvent::SessionClose { id }));
            self.distribute_to_session();
            return;
        }

        debug!("close service session [{}]", id);

        // Close all open proto
        let close_proto_ids = self.session_service_protos.remove(&id).unwrap_or_default();
        debug!("session [{}] close proto [{:?}]", id, close_proto_ids);
        // clean write buffer
        self.write_buf.retain(|&(session_id, _)| id != session_id);
        self.high_write_buf
            .retain(|&(session_id, _)| id != session_id);
        self.set_delay();
        if !self.config.keep_buffer {
            self.read_session_buf
                .retain(|&(session_id, _, _)| id != session_id);
            self.read_service_buf
                .retain(|&(session_id, _, _)| Some(id) != session_id);
        }

        close_proto_ids.into_iter().for_each(|proto_id| {
            self.protocol_close(id, proto_id, Source::Internal);
        });

        if let Some(session_control) = self.sessions.remove(&id) {
            // Service handle processing flow
            self.handle.handle_event(
                &mut self.service_context,
                ServiceEvent::SessionClose {
                    session_context: session_control.inner,
                },
            );
        }
    }

    /// Open the handle corresponding to the protocol
    #[inline]
    fn protocol_open(
        &mut self,
        id: SessionId,
        proto_id: ProtocolId,
        version: String,
        source: Source,
    ) {
        if source == Source::External {
            debug!("try open session [{}] proto [{}]", id, proto_id);
            // The following 3 conditions must be met at the same time to send an event:
            //
            // 1. session must open
            // 2. session protocol mustn't open
            // 3. session protocol handle mustn't exist
            //
            // Satisfy 2 but not 3 may cause an error, leading to the service's session handle sender
            // to be inconsistent with the substream's sender, opened two different session protocol handles
            if self.sessions.contains_key(&id)
                && !self
                    .session_service_protos
                    .get(&id)
                    .map(|protos| protos.contains(&proto_id))
                    .unwrap_or_default()
                && !self.session_proto_handles.contains_key(&(id, proto_id))
            {
                if let Some(handle) = self.proto_handle(true, proto_id) {
                    self.handle_open(handle, proto_id, Some(self.next_session))
                };
                self.write_buf.push_back((
                    id,
                    SessionEvent::ProtocolOpen {
                        id,
                        proto_id,
                        version,
                        session_sender: self.session_proto_handles.get(&(id, proto_id)).cloned(),
                    },
                ));
                self.distribute_to_session();
            }
            return;
        }

        debug!("service session [{}] proto [{}] open", id, proto_id);

        // Regardless of the existence of the session level handle,
        // you **must record** which protocols are opened for each session.
        self.session_service_protos
            .entry(id)
            .or_default()
            .insert(proto_id);

        if self.config.event.contains(&proto_id) {
            if let Some(session_control) = self.sessions.get(&id) {
                // event output
                self.handle.handle_proto(
                    &mut self.service_context,
                    ProtocolEvent::Connected {
                        session_context: Arc::clone(&session_control.inner),
                        proto_id,
                        version: version.clone(),
                    },
                );
            }
        }
    }

    /// Processing the received data
    #[inline]
    fn protocol_message(
        &mut self,
        session_id: SessionId,
        proto_id: ProtocolId,
        data: bytes::Bytes,
    ) {
        debug!(
            "service receive session [{}] proto [{}] data len: {}",
            session_id,
            proto_id,
            data.len()
        );

        if self.config.event.contains(&proto_id) {
            if let Some(session_control) = self.sessions.get(&session_id) {
                // event output
                self.handle.handle_proto(
                    &mut self.service_context,
                    ProtocolEvent::Received {
                        session_context: Arc::clone(&session_control.inner),
                        proto_id,
                        data: data.clone(),
                    },
                );
            }
        }
    }

    /// Protocol stream is closed, clean up data
    #[inline]
    fn protocol_close(&mut self, session_id: SessionId, proto_id: ProtocolId, source: Source) {
        if source == Source::External {
            debug!("try close session [{}] proto [{}]", session_id, proto_id);
            self.write_buf.push_back((
                session_id,
                SessionEvent::ProtocolClose {
                    id: session_id,
                    proto_id,
                },
            ));
            self.distribute_to_session();
            return;
        }

        debug!(
            "service session [{}] proto [{}] close",
            session_id, proto_id
        );

        if self.config.event.contains(&proto_id) {
            if let Some(session_control) = self.sessions.get(&session_id) {
                self.handle.handle_proto(
                    &mut self.service_context,
                    ProtocolEvent::Disconnected {
                        proto_id,
                        session_context: Arc::clone(&session_control.inner),
                    },
                )
            }
        }

        // Session proto info remove
        if let Some(infos) = self.session_service_protos.get_mut(&session_id) {
            infos.remove(&proto_id);
        }
        self.session_proto_handles.remove(&(session_id, proto_id));
    }

    #[inline(always)]
    fn send_pending_task(&mut self) {
        while let Some(task) = self.pending_tasks.pop_front() {
            if let Err(err) = self.future_task_sender.try_send(task) {
                if err.is_full() {
                    self.pending_tasks.push_front(err.into_inner());
                    self.set_delay();
                    break;
                }
            }
        }
    }

    #[inline]
    fn send_future_task(&mut self, task: BoxedFutureTask) {
        let task = Box::new(BlockingFutureTask::new(task));
        self.pending_tasks.push_back(task);
        self.send_pending_task();
    }

    /// check queue if full
    fn notify_queue(&mut self) {
        let notify = futures::task::current();
        let quick_count = self.service_context.control().quick_count.clone();
        let normal_count = self.service_context.control().normal_count.clone();
        let task = Interval::new(Instant::now(), Duration::from_millis(200))
            .map_err(|_| ())
            .for_each(move |_| {
                if quick_count.load(Ordering::SeqCst) > RECEIVED_BUFFER_SIZE / 4
                    || normal_count.load(Ordering::SeqCst) > RECEIVED_BUFFER_SIZE / 2
                {
                    notify.notify();
                }
                Ok(())
            })
            .map_err(|_| debug!("queue notify close"));
        self.send_future_task(Box::new(task))
    }

    fn init_proto_handles(&mut self) {
        let ids = self
            .protocol_configs
            .values_mut()
            .map(|meta| (meta.id(), meta.before_send.take()))
            .collect::<Vec<(ProtocolId, _)>>();
        for (id, before_send) in ids {
            if let Some(handle) = self.proto_handle(false, id) {
                self.handle_open(handle, id, None);
            }
            if let Some(function) = before_send {
                self.before_sends.insert(id, function);
            }
        }
    }

    /// When listen update, call here
    #[inline]
    fn update_listens(&mut self) {
        if self.listens.len() == self.service_context.listens().len() {
            return;
        }
        let new_listens = self
            .listens
            .iter()
            .map(|(address, _)| address.clone())
            .collect::<Vec<Multiaddr>>();
        self.service_context.update_listens(new_listens.clone());

        for proto_id in self.service_proto_handles.keys() {
            self.read_service_buf.push_back((
                None,
                *proto_id,
                ServiceProtocolEvent::Update {
                    listen_addrs: new_listens.clone(),
                },
            ));
        }

        for (session_id, proto_id) in self.session_proto_handles.keys() {
            self.read_session_buf.push_back((
                *session_id,
                *proto_id,
                SessionProtocolEvent::Update {
                    listen_addrs: new_listens.clone(),
                },
            ));
        }

        self.distribute_to_user_level();
    }

    /// Handling various events uploaded by the session
    fn handle_session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::SessionClose { id } => self.session_close(id, Source::Internal),
            SessionEvent::HandshakeSuccess {
                handle,
                public_key,
                address,
                ty,
            } => {
                self.session_open(handle, Some(public_key), address, ty);
            }
            SessionEvent::HandshakeFail { ty, error, address } => {
                if ty.is_outbound() {
                    self.state.decrease();
                    self.dial_protocols.remove(&address);
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::DialerError { address, error },
                    )
                }
            }
            SessionEvent::ProtocolMessage {
                id, proto_id, data, ..
            } => self.protocol_message(id, proto_id, data),
            SessionEvent::ProtocolOpen {
                id,
                proto_id,
                version,
                ..
            } => self.protocol_open(id, proto_id, version, Source::Internal),
            SessionEvent::ProtocolClose { id, proto_id } => {
                self.protocol_close(id, proto_id, Source::Internal)
            }
            SessionEvent::ProtocolSelectError { id, proto_name } => {
                if let Some(session_control) = self.sessions.get(&id) {
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ProtocolSelectError {
                            proto_name,
                            session_context: Arc::clone(&session_control.inner),
                        },
                    )
                }
            }
            SessionEvent::ProtocolError {
                id,
                proto_id,
                error,
            } => self.handle.handle_error(
                &mut self.service_context,
                ServiceError::ProtocolError {
                    id,
                    proto_id,
                    error,
                },
            ),
            SessionEvent::DialError { address, error } => {
                self.state.decrease();
                self.dial_protocols.remove(&address);
                self.handle.handle_error(
                    &mut self.service_context,
                    ServiceError::DialerError { address, error },
                )
            }
            SessionEvent::ListenError { address, error } => {
                self.state.decrease();
                self.handle.handle_error(
                    &mut self.service_context,
                    ServiceError::ListenError { address, error },
                )
            }
            SessionEvent::SessionTimeout { id } => {
                if let Some(session_control) = self.sessions.get(&id) {
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::SessionTimeout {
                            session_context: Arc::clone(&session_control.inner),
                        },
                    )
                }
            }
            SessionEvent::MuxerError { id, error } => {
                if let Some(session_control) = self.sessions.get(&id) {
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::MuxerError {
                            session_context: Arc::clone(&session_control.inner),
                            error,
                        },
                    )
                }
            }
            SessionEvent::ListenStart {
                listen_address,
                incoming,
            } => {
                self.handle.handle_event(
                    &mut self.service_context,
                    ServiceEvent::ListenStarted {
                        address: listen_address.clone(),
                    },
                );
                if let Some(client) = self.igd_client.as_mut() {
                    client.register(&listen_address)
                }
                self.listens.push((listen_address, incoming));
                self.state.decrease();
                self.update_listens();
                self.listen_poll();
            }
            SessionEvent::DialStart {
                remote_address,
                stream,
            } => self.handshake(stream, SessionType::Outbound, remote_address),
        }
    }

    /// Handling various tasks sent externally
    fn handle_service_task(&mut self, event: ServiceTask) {
        match event {
            ServiceTask::ProtocolMessage {
                target,
                proto_id,
                priority,
                data,
            } => {
                let data = match self.before_sends.get(&proto_id) {
                    Some(function) => function(data),
                    None => data,
                };
                match target {
                    TargetSession::Single(id) => self.send_message_to(id, proto_id, priority, data),
                    TargetSession::Multi(ids) => {
                        self.filter_broadcast(ids, proto_id, priority, data)
                    }
                    TargetSession::All => self.broadcast(proto_id, priority, data),
                }
            }
            ServiceTask::Dial { address, target } => {
                if !self.dial_protocols.contains_key(&address) {
                    if let Err(e) = self.dial_inner(address.clone(), target) {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::DialerError {
                                address,
                                error: e.into(),
                            },
                        );
                    }
                }
            }
            ServiceTask::Listen { address } => {
                if !self.listens.iter().any(|(addr, _)| addr == &address) {
                    if let Err(e) = self.listen(address.clone()) {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::ListenError {
                                address,
                                error: e.into(),
                            },
                        );
                    }
                }
            }
            ServiceTask::Disconnect { session_id } => {
                self.session_close(session_id, Source::External)
            }
            ServiceTask::FutureTask { task } => {
                self.send_future_task(task);
            }
            ServiceTask::SetProtocolNotify {
                proto_id,
                interval,
                token,
            } => {
                // TODO: if not contains should call handle_error let user know
                if self.service_proto_handles.contains_key(&proto_id) {
                    self.read_service_buf.push_back((
                        None,
                        proto_id,
                        ServiceProtocolEvent::SetNotify { interval, token },
                    ));
                    self.distribute_to_user_level();
                }
            }
            ServiceTask::RemoveProtocolNotify { proto_id, token } => {
                if self.service_proto_handles.contains_key(&proto_id) {
                    self.read_service_buf.push_back((
                        None,
                        proto_id,
                        ServiceProtocolEvent::RemoveNotify { token },
                    ));
                    self.distribute_to_user_level();
                }
            }
            ServiceTask::SetProtocolSessionNotify {
                session_id,
                proto_id,
                interval,
                token,
            } => {
                // TODO: if not contains should call handle_error let user know
                if self
                    .session_proto_handles
                    .contains_key(&(session_id, proto_id))
                {
                    self.read_session_buf.push_back((
                        session_id,
                        proto_id,
                        SessionProtocolEvent::SetNotify { interval, token },
                    ));
                    self.distribute_to_user_level();
                }
            }
            ServiceTask::RemoveProtocolSessionNotify {
                session_id,
                proto_id,
                token,
            } => {
                if self
                    .session_proto_handles
                    .contains_key(&(session_id, proto_id))
                {
                    self.read_session_buf.push_back((
                        session_id,
                        proto_id,
                        SessionProtocolEvent::RemoveNotify { token },
                    ));
                }
            }
            ServiceTask::ProtocolOpen { session_id, target } => match target {
                TargetProtocol::All => {
                    let ids = self
                        .protocol_configs
                        .values()
                        .map(ProtocolMeta::id)
                        .collect::<Vec<_>>();
                    ids.into_iter().for_each(|id| {
                        self.protocol_open(session_id, id, String::default(), Source::External)
                    });
                }
                TargetProtocol::Single(id) => {
                    self.protocol_open(session_id, id, String::default(), Source::External)
                }
                TargetProtocol::Multi(ids) => ids.into_iter().for_each(|id| {
                    self.protocol_open(session_id, id, String::default(), Source::External)
                }),
            },
            ServiceTask::ProtocolClose {
                session_id,
                proto_id,
            } => self.protocol_close(session_id, proto_id, Source::External),
            ServiceTask::Shutdown(quick) => {
                self.state.pre_shutdown();

                while let Some((address, incoming)) = self.listens.pop() {
                    drop(incoming);
                    self.handle.handle_event(
                        &mut self.service_context,
                        ServiceEvent::ListenClose { address },
                    )
                }
                // clear upnp register
                if let Some(client) = self.igd_client.as_mut() {
                    client.clear()
                };
                self.pending_tasks.clear();

                let sessions = self.sessions.keys().cloned().collect::<Vec<SessionId>>();

                if quick {
                    self.quick_task_receiver.close();
                    self.service_task_receiver.close();
                    self.session_event_receiver.close();
                    // clean buffer
                    self.write_buf.clear();
                    self.read_session_buf.clear();
                    self.read_service_buf.clear();
                    self.service_proto_handles.clear();
                    self.session_proto_handles.clear();

                    // don't care about any session action
                    sessions
                        .into_iter()
                        .for_each(|i| self.session_close(i, Source::Internal));
                } else {
                    sessions
                        .into_iter()
                        .for_each(|i| self.session_close(i, Source::External));
                }
            }
        }
    }

    /// Poll listen connections
    #[inline]
    fn listen_poll(&mut self) {
        let mut update = false;
        for (address, mut listen) in self.listens.split_off(0) {
            match listen.poll() {
                Ok(Async::Ready(Some((remote_address, socket)))) => {
                    self.handshake(socket, SessionType::Inbound, remote_address);
                    self.listens.push((address, listen));
                }
                Ok(Async::Ready(None)) => {
                    update = true;
                    if let Some(client) = self.igd_client.as_mut() {
                        client.remove(&address)
                    }
                    self.handle.handle_event(
                        &mut self.service_context,
                        ServiceEvent::ListenClose { address },
                    );
                }
                Ok(Async::NotReady) => {
                    self.listens.push((address, listen));
                }
                Err(err) => {
                    update = true;
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ListenError {
                            address: address.clone(),
                            error: err.into(),
                        },
                    );
                    if let Some(client) = self.igd_client.as_mut() {
                        client.remove(&address)
                    }
                    self.handle.handle_event(
                        &mut self.service_context,
                        ServiceEvent::ListenClose { address },
                    );
                }
            }
        }

        if update || self.service_context.listens().is_empty() {
            self.update_listens()
        }

        if let Some(client) = self.igd_client.as_mut() {
            client.process_only_leases_support()
        }
    }

    #[inline]
    fn user_task_poll(&mut self) {
        let mut finished = false;
        for _ in 0..512 {
            if self.write_buf.len() > self.config.yamux_config.send_event_size()
                && self.high_write_buf.len() > self.config.yamux_config.send_event_size()
            {
                break;
            }

            let task = match self.quick_task_receiver.poll() {
                Ok(Async::Ready(Some(task))) => {
                    self.service_context.control().quick_count_sub();
                    Some(task)
                }
                Ok(Async::Ready(None)) => None,
                Ok(Async::NotReady) => None,
                Err(_) => None,
            }
            .or_else(|| {
                if self.write_buf.len() <= self.config.yamux_config.send_event_size() {
                    match self.service_task_receiver.poll() {
                        Ok(Async::Ready(Some(task))) => {
                            self.service_context.control().normal_count_sub();
                            Some(task)
                        }
                        Ok(Async::Ready(None)) => None,
                        Ok(Async::NotReady) => None,
                        Err(_) => None,
                    }
                } else {
                    None
                }
            });

            match task {
                Some(task) => self.handle_service_task(task),
                None => {
                    finished = true;
                    break;
                }
            }
        }
        if !finished {
            self.set_delay();
        }
    }

    fn session_poll(&mut self) {
        let mut finished = false;
        for _ in 0..64 {
            if self.read_service_buf.len() > self.config.yamux_config.recv_event_size()
                || self.read_session_buf.len() > self.config.yamux_config.recv_event_size()
            {
                break;
            }

            match self.session_event_receiver.poll() {
                Ok(Async::Ready(Some(event))) => self.handle_session_event(event),
                Ok(Async::Ready(None)) => unreachable!(),
                Ok(Async::NotReady) => {
                    finished = true;
                    break;
                }
                Err(_) => {
                    warn!("receive session error");
                    finished = true;
                    break;
                }
            }
        }
        if !finished {
            self.set_delay();
        }
    }

    #[inline]
    fn set_delay(&mut self) {
        // Why use `delay` instead of `notify`?
        //
        // In fact, on machines that can use multi-core normally, there is almost no problem with the `notify` behavior,
        // and even the efficiency will be higher.
        //
        // However, if you are on a single-core bully machine, `notify` may have a very amazing starvation behavior.
        //
        // Under a single-core machine, `notify` may fall into the loop of infinitely preemptive CPU, causing starvation.
        if !self.delay.load(Ordering::SeqCst) {
            self.delay.store(true, Ordering::SeqCst);
            let notify = futures::task::current();
            let delay = self.delay.clone();
            let delay_task = Delay::new(Instant::now() + DELAY_TIME).then(move |_| {
                notify.notify();
                delay.store(false, Ordering::SeqCst);
                Ok(())
            });

            tokio::spawn(delay_task);
        }
    }
}

impl<T> Stream for Service<T>
where
    T: ServiceHandle,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.listens.is_empty()
            && self.state.is_shutdown()
            && self.sessions.is_empty()
            && self.pending_tasks.is_empty()
        {
            debug!("shutdown because all state is empty head");
            self.shutdown.store(true, Ordering::SeqCst);
            return Ok(Async::Ready(None));
        }

        if let Some(stream) = self.future_task_manager.take() {
            tokio::spawn(stream.for_each(|_| Ok(())));
            self.notify_queue();
            self.init_proto_handles();
        }

        if !self.write_buf.is_empty() || !self.high_write_buf.is_empty() {
            self.distribute_to_session();
        }
        if !self.read_service_buf.is_empty() || !self.read_session_buf.is_empty() {
            self.distribute_to_user_level();
        }

        self.listen_poll();

        self.session_poll();

        // receive user task
        self.user_task_poll();

        // process any task buffer
        self.send_pending_task();

        // Double check service state
        if self.listens.is_empty()
            && self.state.is_shutdown()
            && self.sessions.is_empty()
            && self.pending_tasks.is_empty()
        {
            debug!("shutdown because all state is empty tail");
            self.shutdown.store(true, Ordering::SeqCst);
            return Ok(Async::Ready(None));
        }

        debug!(
            "> listens count: {}, state: {:?}, sessions count: {}, \
             pending task: {}, normal_count: {}, quick_count: {}, high_write_buf: {}, write_buf: {}, read_service_buf: {}, read_session_buf: {}",
            self.listens.len(),
            self.state,
            self.sessions.len(),
            self.pending_tasks.len(),
            self.service_context
                .control()
                .normal_count
                .load(Ordering::SeqCst),
            self.service_context
                .control()
                .quick_count
                .load(Ordering::SeqCst),
            self.high_write_buf.len(),
            self.write_buf.len(),
            self.read_service_buf.len(),
            self.read_session_buf.len(),
        );

        Ok(Async::NotReady)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Source {
    /// Event from user
    External,
    /// Event from session
    Internal,
}

/// Indicates the session type
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SessionType {
    /// Representing yourself as the active party means that you are the client side
    Outbound,
    /// Representing yourself as a passive recipient means that you are the server side
    Inbound,
}

impl SessionType {
    /// is outbound
    #[inline]
    pub fn is_outbound(self) -> bool {
        match self {
            SessionType::Outbound => true,
            SessionType::Inbound => false,
        }
    }

    /// is inbound
    #[inline]
    pub fn is_inbound(self) -> bool {
        !self.is_outbound()
    }
}

impl From<YamuxType> for SessionType {
    #[inline]
    fn from(ty: YamuxType) -> Self {
        match ty {
            YamuxType::Client => SessionType::Outbound,
            YamuxType::Server => SessionType::Inbound,
        }
    }
}

impl Into<YamuxType> for SessionType {
    #[inline]
    fn into(self) -> YamuxType {
        match self {
            SessionType::Outbound => YamuxType::Client,
            SessionType::Inbound => YamuxType::Server,
        }
    }
}
