//! Witness that `Client::new_with_deps_local` accepts a [`LocalSpawner`]
//! and returns a (possibly `!Send`) run-loop future. Sibling test file
//! to `bare_metal_client.rs` — kept separate so it has its own static
//! channel pool and can't collide with the Send-flavored Client
//! construction witness when cargo runs the tests in parallel.
#![cfg(all(feature = "client", feature = "bare_metal"))]

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use simple_someip::client::Error as ClientError;
use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
use simple_someip::define_static_channels;
use simple_someip::e2e::E2ERegistry;
use simple_someip::protocol::sd::RebootFlag;
use simple_someip::transport::{
    LocalSpawner, ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory,
    TransportSocket,
};
use simple_someip::{Client, ClientDeps, RawPayload};

define_static_channels! {
    name: LocalChannels,
    oneshot: [
        (Result<(), ClientError>, 4),
        (Result<RawPayload, ClientError>, 2),
        (Result<RebootFlag, ClientError>, 2),
    ],
    bounded: [
        ((ControlMessage<RawPayload, LocalChannels>, 4), 2),
        ((SendMessage<RawPayload, LocalChannels>, 16), 2),
        ((Result<ReceivedMessage<RawPayload>, ClientError>, 16), 2),
    ],
    unbounded: [
        (ClientUpdate<RawPayload>, 2),
    ],
}

// ── Mock transport (mirrors bare_metal_client.rs) ─────────────────────

#[derive(Default)]
struct MockPipe {
    sent: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound_waker: Mutex<Option<core::task::Waker>>,
}

#[derive(Clone)]
struct MockFactory {
    pipe: Arc<MockPipe>,
    local_port: Arc<Mutex<u16>>,
}

impl TransportFactory for MockFactory {
    type Socket = MockSocket;
    type BindFuture<'a> =
        core::pin::Pin<Box<dyn Future<Output = Result<Self::Socket, TransportError>> + 'a>>;
    fn bind<'a>(&'a self, addr: SocketAddrV4, _options: &'a SocketOptions) -> Self::BindFuture<'a> {
        let pipe = Arc::clone(&self.pipe);
        let mut p = self.local_port.lock().unwrap();
        let port = if addr.port() == 0 {
            let next = *p + 1;
            *p = next;
            40000 + next
        } else {
            addr.port()
        };
        let local = SocketAddrV4::new(*addr.ip(), port);
        Box::pin(async move { Ok(MockSocket { pipe, local }) })
    }
}

struct MockSocket {
    pipe: Arc<MockPipe>,
    local: SocketAddrV4,
}

struct MockSendFut {
    pipe: Arc<MockPipe>,
    bytes: Option<Vec<u8>>,
    target: SocketAddrV4,
}

impl Future for MockSendFut {
    type Output = Result<(), TransportError>;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        if let Some(bytes) = me.bytes.take() {
            me.pipe.sent.lock().unwrap().push_back((bytes, me.target));
        }
        Poll::Ready(Ok(()))
    }
}

struct MockRecvFut<'a> {
    pipe: Arc<MockPipe>,
    buf: &'a mut [u8],
}

impl Future for MockRecvFut<'_> {
    type Output = Result<ReceivedDatagram, TransportError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        let entry = me.pipe.inbound.lock().unwrap().pop_front();
        match entry {
            Some((bytes, source)) => {
                let n = bytes.len().min(me.buf.len());
                me.buf[..n].copy_from_slice(&bytes[..n]);
                Poll::Ready(Ok(ReceivedDatagram {
                    bytes_received: n,
                    source,
                    truncated: n < bytes.len(),
                }))
            }
            None => {
                *me.pipe.inbound_waker.lock().unwrap() = Some(cx.waker().clone());
                if let Some((bytes, source)) = me.pipe.inbound.lock().unwrap().pop_front() {
                    let n = bytes.len().min(me.buf.len());
                    me.buf[..n].copy_from_slice(&bytes[..n]);
                    return Poll::Ready(Ok(ReceivedDatagram {
                        bytes_received: n,
                        source,
                        truncated: n < bytes.len(),
                    }));
                }
                Poll::Pending
            }
        }
    }
}

impl TransportSocket for MockSocket {
    type SendFuture<'a> = MockSendFut;
    type RecvFuture<'a> = MockRecvFut<'a>;

    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
        MockSendFut {
            pipe: Arc::clone(&self.pipe),
            bytes: Some(buf.to_vec()),
            target,
        }
    }

    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        MockRecvFut {
            pipe: Arc::clone(&self.pipe),
            buf,
        }
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        Ok(self.local)
    }

    fn join_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }
    fn leave_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }
}

struct MockTimer;
impl Timer for MockTimer {
    type SleepFuture<'a> = core::pin::Pin<Box<dyn Future<Output = ()> + 'a>>;
    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

struct LocalTokioSpawner;
impl LocalSpawner for LocalTokioSpawner {
    fn spawn_local(&self, future: impl Future<Output = ()> + 'static) {
        drop(tokio::task::spawn_local(future));
    }
}

#[tokio::test]
async fn client_constructible_with_local_spawner() {
    tokio::task::LocalSet::new()
        .run_until(async move {
            let pipe = Arc::new(MockPipe::default());
            let factory = MockFactory {
                pipe: Arc::clone(&pipe),
                local_port: Arc::new(Mutex::new(0)),
            };

            let interface_handle: Arc<std::sync::RwLock<Ipv4Addr>> =
                Arc::new(std::sync::RwLock::new(Ipv4Addr::LOCALHOST));
            let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));

            let (client, _updates, run_fut) = Client::<
                RawPayload,
                Arc<Mutex<E2ERegistry>>,
                Arc<std::sync::RwLock<Ipv4Addr>>,
                LocalChannels,
            >::new_with_deps_local(
                ClientDeps {
                    factory,
                    spawner: LocalTokioSpawner,
                    timer: MockTimer,
                    e2e_registry: e2e_handle,
                    interface: interface_handle,
                },
                false,
            );

            let run_handle = tokio::task::spawn_local(run_fut);
            assert_eq!(client.interface(), Ipv4Addr::LOCALHOST);

            run_handle.abort();
            drop(client);
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
}
