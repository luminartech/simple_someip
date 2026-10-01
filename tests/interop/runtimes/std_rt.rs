//! `SomeipUnderTest` over simple-someip's tokio `Client` and `Server`.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use simple_someip::e2e::{E2ECheckStatus, E2EKey, E2EProfile, E2ERegistry, Profile5Config};
use simple_someip::protocol::sd::{Entry, Options, ServiceEntry, TransportProtocol};
use simple_someip::protocol::{
    Header, Message, MessageId, MessageType, MessageTypeField, ReturnCode,
};
use simple_someip::server::{EventPublisher, ServerConfig, SubscriptionManager};
use simple_someip::{
    Client, ClientUpdate, ClientUpdates, PayloadWireFormat, RawPayload, Server, ServerDeps,
    ServiceEndpointKey, TokioChannels, TokioSocket,
};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::interop::consts::*;
use crate::interop::runtime::*;

type E2eHandle = Arc<Mutex<E2ERegistry>>;
type StdClient = Client<RawPayload, E2eHandle, Arc<std::sync::RwLock<Ipv4Addr>>, TokioChannels>;
type Publisher = Arc<
    EventPublisher<
        E2eHandle,
        Arc<tokio::sync::RwLock<SubscriptionManager>>,
        Arc<TokioSocket>,
        TokioSocket,
    >,
>;

/// TTL of the SubscribeEventgroup entries the client sends.
const SUBSCRIBE_TTL_S: u32 = 3;

/// Where the server's request callback (a plain `fn`) reports requests.
static REQUESTS: Mutex<Option<mpsc::Sender<Observation>>> = Mutex::new(None);

pub struct StdRt {
    rt: Runtime,
    obs: mpsc::Receiver<Observation>,
    client: Option<StdClient>,
    /// The discovered `SVC`/`INST` endpoint, set by the update task.
    service: Arc<Mutex<Option<ServiceEndpointKey>>>,
    publisher: Option<Publisher>,
    event_session: u16,
    tasks: Vec<JoinHandle<()>>,
}

impl SomeipUnderTest for StdRt {
    const NAME: &'static str = "std runtime";

    fn start(setup: Setup) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("could not build the tokio runtime");
        let (tx, obs) = mpsc::channel();
        let service = Arc::new(Mutex::new(None));
        let mut tasks = Vec::new();
        let client = setup.consume.map(|consume| {
            rt.block_on(start_client(
                consume,
                tx.clone(),
                service.clone(),
                &mut tasks,
            ))
        });
        let publisher = setup
            .offer
            .map(|offer| rt.block_on(start_server(&offer, tx.clone(), &mut tasks)));
        Self {
            rt,
            obs,
            client,
            service,
            publisher,
            event_session: 0,
            tasks,
        }
    }

    fn stop_offer(&mut self) {
        unsupported(
            Self::NAME,
            "stopping an offer (the Server has no stop-offer API)",
        )
    }

    fn publish(&mut self, event: u16, payload: &[u8]) {
        self.event_session = self.event_session.wrapping_add(1).max(1);
        let request_id = u32::from(self.event_session);
        let publisher = self
            .publisher
            .as_ref()
            .unwrap_or_else(|| panic!("{}: publish without an offer", Self::NAME));
        self.rt
            .block_on(
                publisher.publish_raw_event(SVC, INST, EG, event, request_id, 1, MAJOR, payload),
            )
            .unwrap_or_else(|e| panic!("{}: publishing 0x{event:04X} failed: {e:?}", Self::NAME));
    }

    /// The Server has no field support (no initial value on subscribe), so a
    /// field update is published like an event.
    fn set_field(&mut self, field: u16, payload: &[u8]) {
        self.publish(field, payload);
    }

    fn unsubscribe(&mut self) {
        unsupported(
            Self::NAME,
            "unsubscribing (the Client has no unsubscribe API)",
        )
    }

    fn call(&mut self, method: u16, payload: &[u8], timeout: Duration) -> CallOutcome {
        let (client, key) = self.client_and_service();
        let msg = request(method, payload, MessageType::Request);
        match self
            .rt
            .block_on(tokio::time::timeout(timeout, client.request(key, msg)))
        {
            Err(_elapsed) => CallOutcome::NoReply,
            // The Client returns only the response payload, not its return code.
            Ok(Ok(p)) => CallOutcome::Response {
                return_code: 0,
                payload: p.raw_bytes().unwrap_or_default().to_vec(),
            },
            Ok(Err(e)) => panic!("{}: request 0x{method:04X} failed: {e:?}", Self::NAME),
        }
    }

    fn fire_and_forget(&mut self, method: u16, payload: &[u8]) {
        let (client, key) = self.client_and_service();
        let msg = request(method, payload, MessageType::RequestNoReturn);
        let pending = self
            .rt
            .block_on(client.send_to_service(key, msg))
            .unwrap_or_else(|e| panic!("{}: sending 0x{method:04X} failed: {e:?}", Self::NAME));
        drop(pending);
    }

    fn next(&mut self, timeout: Duration) -> Option<Observation> {
        self.obs.recv_timeout(timeout).ok()
    }
}

impl StdRt {
    fn client_and_service(&self) -> (&StdClient, ServiceEndpointKey) {
        let client = self
            .client
            .as_ref()
            .unwrap_or_else(|| panic!("{}: method call without a consumer", Self::NAME));
        let key =
            self.service.lock().unwrap().unwrap_or_else(|| {
                panic!("{}: method call before 0x{SVC:04X} was found", Self::NAME)
            });
        (client, key)
    }
}

impl Drop for StdRt {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.shut_down();
        }
        for task in &self.tasks {
            task.abort();
        }
        if self.publisher.is_some() {
            *REQUESTS.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}

async fn start_client(
    consume: Consume,
    tx: mpsc::Sender<Observation>,
    service: Arc<Mutex<Option<ServiceEndpointKey>>>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> StdClient {
    let (client, updates, run) = StdClient::new_with_loopback(OUR_IP, true);
    tasks.push(tokio::spawn(run));
    client
        .bind_discovery()
        .await
        .expect("std runtime: bind_discovery failed");
    if let Some(e2e) = &consume.e2e {
        let max_delta = u8::try_from(e2e.max_delta).expect("Profile 5 max_delta fits in a u8");
        client
            .register_e2e(
                E2EKey::from_message_id(MessageId::new_from_service_and_method(SVC, EVENT)),
                E2EProfile::Profile5WithHeader(Profile5Config::new(
                    e2e.data_id,
                    e2e.data_length_bits,
                    max_delta,
                )),
            )
            .expect("std runtime: E2E registry is full");
    }
    tasks.push(tokio::spawn(report_updates(
        client.clone(),
        updates,
        consume.subscribe,
        tx,
        service,
    )));
    client
}

/// Turns client updates into observations. When `SVC`/`INST` appears, it
/// registers the endpoint (so method calls can reach it) and, if asked,
/// subscribes to `EG` — before reporting it, so a scenario that sees
/// `ServiceAvailable` can call straight away.
async fn report_updates(
    client: StdClient,
    mut updates: ClientUpdates<RawPayload, TokioChannels>,
    subscribe: bool,
    tx: mpsc::Sender<Observation>,
    service: Arc<Mutex<Option<ServiceEndpointKey>>>,
) {
    let mut available = HashSet::new();
    while let Some(update) = updates.recv().await {
        let mut out = Vec::new();
        match update {
            ClientUpdate::DiscoveryUpdated(msg) => {
                // With multicast loopback on, the client also hears our own SD.
                if msg.source.ip() == IpAddr::V4(OUR_IP) {
                    continue;
                }
                for entry in &msg.sd_header.entries {
                    match entry {
                        Entry::OfferService(s) if s.ttl > 0 => {
                            let Some(endpoint) = udp_endpoint(s, &msg.sd_header.options) else {
                                eprintln!("std runtime: offer without a UDP endpoint: {s:?}");
                                continue;
                            };
                            let first = available.insert((s.service_id, s.instance_id));
                            if first && s.service_id == SVC && s.instance_id == INST {
                                let key = ServiceEndpointKey::udp(SVC, SocketAddr::V4(endpoint));
                                use_service(&client, key, subscribe).await;
                                *service.lock().unwrap() = Some(key);
                            }
                            out.push(Observation::ServiceAvailable {
                                service: s.service_id,
                                instance: s.instance_id,
                                endpoint,
                            });
                        }
                        Entry::OfferService(s) | Entry::StopOfferService(s) => {
                            available.remove(&(s.service_id, s.instance_id));
                            if s.service_id == SVC && s.instance_id == INST {
                                *service.lock().unwrap() = None;
                            }
                            out.push(Observation::ServiceGone {
                                service: s.service_id,
                                instance: s.instance_id,
                            });
                        }
                        Entry::SubscribeAckEventGroup(eg) => out.push(Observation::Subscribed {
                            eventgroup: eg.event_group_id,
                            accepted: eg.ttl > 0,
                        }),
                        _ => {}
                    }
                }
            }
            ClientUpdate::Unicast {
                message,
                e2e_status,
                ..
            } if message.header().message_type().message_type() == MessageType::Notification => {
                let id = message.header().message_id();
                out.push(Observation::Event {
                    service: id.service_id(),
                    event: id.method_id(),
                    payload: message.payload().raw_bytes().unwrap_or_default().to_vec(),
                    e2e_ok: e2e_status.map(|s| s == E2ECheckStatus::Ok),
                });
            }
            _ => {}
        }
        for o in out {
            if tx.send(o).is_err() {
                return;
            }
        }
    }
}

async fn use_service(client: &StdClient, key: ServiceEndpointKey, subscribe: bool) {
    if let Err(e) = client.add_endpoint(key, INST, CLIENT_PORT).await {
        eprintln!("std runtime: add_endpoint failed: {e:?}");
        return;
    }
    if subscribe
        && let Err(e) = client
            .subscribe(key, MAJOR, SUBSCRIBE_TTL_S, EG, CLIENT_PORT)
            .await
    {
        eprintln!("std runtime: subscribe failed: {e:?}");
    }
}

/// The UDP IPv4 endpoint among the options `entry` references.
fn udp_endpoint(entry: &ServiceEntry, options: &[Options]) -> Option<SocketAddrV4> {
    let run = |first: u8, count: u8| usize::from(first)..usize::from(first) + usize::from(count);
    let runs = run(
        entry.index_first_options_run,
        entry.options_count.first_options_count,
    )
    .chain(run(
        entry.index_second_options_run,
        entry.options_count.second_options_count,
    ));
    runs.filter_map(|i| options.get(i)).find_map(|o| match o {
        Options::IpV4Endpoint {
            ip,
            protocol: TransportProtocol::Udp,
            port,
        } => Some(SocketAddrV4::new(*ip, *port)),
        _ => None,
    })
}

async fn start_server(
    offer: &Offer,
    tx: mpsc::Sender<Observation>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Publisher {
    *REQUESTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let config = ServerConfig::new(SVC, INST)
        .with_interface(OUR_IP)
        .with_local_port(SERVER_PORT)
        .with_major_version(MAJOR)
        .with_ttl(Duration::from_secs(u64::from(offer.ttl_s)))
        .with_event_group(EG);
    let deps = ServerDeps::tokio().with_non_sd_observer(Some((on_request, 0)));
    let (server, handles, _run): (Server<_, _, _, _>, _, _) =
        Server::new_with_deps(deps, config, true)
            .await
            .expect("std runtime: Server::new_with_deps failed");
    let run = server.run();
    tasks.push(tokio::spawn(async move {
        if let Err(e) = run.await {
            eprintln!("std runtime: server stopped: {e:?}");
        }
    }));
    handles.publisher
}

/// The server's non-SD request callback: reports the request and echoes its
/// payload as the response.
///
/// The callback is not given the message type or request id, so `no_return`
/// and `session` are reported as `false` and `0`, and a fire-and-forget
/// request is answered like any other.
fn on_request(
    _ctx: usize,
    _source: SocketAddrV4,
    service: u16,
    method: u16,
    payload: &[u8],
    _e2e_status: u8,
    response_out: &mut [u8],
) -> i32 {
    if let Ok(guard) = REQUESTS.lock()
        && let Some(tx) = guard.as_ref()
    {
        let _ = tx.send(Observation::Request {
            service,
            method,
            no_return: false,
            session: 0,
            payload: payload.to_vec(),
        });
    }
    let n = payload.len().min(response_out.len());
    response_out[..n].copy_from_slice(&payload[..n]);
    i32::try_from(n).unwrap_or(-1)
}

fn request(method: u16, payload: &[u8], kind: MessageType) -> Message<RawPayload> {
    let id = MessageId::new_from_service_and_method(SVC, method);
    Message::new(
        Header::new(
            id,
            0,
            1,
            MAJOR,
            MessageTypeField::new(kind, false),
            ReturnCode::Ok,
            payload.len(),
        ),
        RawPayload::from_payload_bytes(id, payload).expect("a raw payload always parses"),
    )
}
