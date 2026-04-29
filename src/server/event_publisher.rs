//! Event publishing functionality

use super::Error;
use super::subscription_manager::{SUBSCRIBERS_PER_GROUP, SubscriptionHandle};
use crate::UDP_BUFFER_SIZE;
use crate::e2e::E2EKey;
use crate::protocol::{Header, Message};
use crate::traits::{PayloadWireFormat, WireFormat};
use crate::transport::{E2ERegistryHandle, SharedHandle, TransportSocket};
#[cfg(test)]
use alloc::sync::Arc;
use core::marker::PhantomData;
use core::net::SocketAddrV4;
use heapless::Vec as HeaplessVec;

/// The publish snapshot buffer is sized to `SUBSCRIBERS_PER_GROUP` so
/// `for_each_subscriber` can never overflow it. If a future refactor
/// changes the manager's per-group cap independently, this assert
/// catches the divergence at compile time.
const _: () = assert!(
    SUBSCRIBERS_PER_GROUP >= 1,
    "SUBSCRIBERS_PER_GROUP must be >= 1 for the publish snapshot to fit any subscribers"
);

/// Publishes events to subscribers.
///
/// Generic over `H: SharedHandle<T>` (abstracting how the
/// transport socket is shared — `Arc<T>` in alloc-using builds,
/// `&'static T` on bare-metal-no-alloc), `T: TransportSocket`
/// (the concrete underlying socket type), `R: E2ERegistryHandle`,
/// and `S: SubscriptionHandle`.
///
/// Pre-19f revision: this type held an `Arc<T>` directly and required
/// `T: Send + Sync + 'static`. The handle indirection drops the
/// Send/Sync requirement so consumers with a `!Sync` socket — most
/// notably `embassy-net`'s `UdpSocket<'static>` — can still
/// construct an `EventPublisher`. Multi-threaded callers continue
/// to use `Arc<T>` (which is `Send + Sync` whenever `T` is) without
/// any change.
///
/// The explicit `T` parameter is the price of consolidating all
/// three former handle traits (Phase 20e) into a single
/// [`SharedHandle<T>`]: the trait carries `T` as a generic, not
/// as an associated type, so consumers that need to name the
/// socket type spell it out.
pub struct EventPublisher<R, S, H, T>
where
    R: E2ERegistryHandle,
    S: SubscriptionHandle,
    T: TransportSocket + 'static,
    H: SharedHandle<T>,
{
    subscriptions: S,
    socket: H,
    e2e_registry: R,
    /// `T` appears only in the bound `H: SharedHandle<T>`; the
    /// struct doesn't directly hold a `T`. `PhantomData` carries
    /// the type so the parameter is well-formed without affecting
    /// drop-check or auto-trait propagation negatively.
    _phantom: PhantomData<T>,
}

impl<R, S, H, T> EventPublisher<R, S, H, T>
where
    R: E2ERegistryHandle,
    S: SubscriptionHandle,
    T: TransportSocket + 'static,
    H: SharedHandle<T>,
{
    /// Create a new event publisher.
    ///
    /// `socket` is whatever [`SharedHandle<T>`] impl the caller
    /// chose for storage — `Arc<T>` on std/alloc, `&'static T` on
    /// bare-metal-no-alloc.
    pub fn new(subscriptions: S, socket: H, e2e_registry: R) -> Self {
        Self {
            subscriptions,
            socket,
            e2e_registry,
            _phantom: PhantomData,
        }
    }

    /// Publish an event to all subscribers of an event group
    ///
    /// # Arguments
    /// * `service_id` - Service ID
    /// * `instance_id` - Instance ID
    /// * `event_group_id` - Event group ID
    /// * `message` - The SOME/IP message to send (must be a notification/event)
    ///
    /// # Errors
    ///
    /// Returns an error if the message fails to serialize.
    ///
    /// # Panics
    ///
    /// May panic if the underlying [`E2ERegistryHandle`](crate::transport::E2ERegistryHandle)
    /// implementation panics (e.g., `Arc<Mutex<E2ERegistry>>` on mutex poison).
    #[allow(clippy::too_many_lines)]
    pub async fn publish_event<P: PayloadWireFormat>(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        message: &Message<P>,
    ) -> Result<usize, Error> {
        // Snapshot subscriber addresses into a stack-allocated buffer so
        // we can release the subscription read lock before doing async
        // sends. This avoids a per-event heap allocation that the old
        // `get_subscribers -> Vec<Subscriber>` API forced.
        //
        // The buffer cap matches the manager's per-group cap so push()
        // is provably infallible — see the `const _` guard below.
        let mut subscribers: HeaplessVec<SocketAddrV4, SUBSCRIBERS_PER_GROUP> = HeaplessVec::new();
        let _total = self
            .subscriptions
            .for_each_subscriber(service_id, instance_id, event_group_id, |sub| {
                // push() can never fail here: SUBSCRIBERS_PER_GROUP is
                // both the manager's per-group cap and this buffer's
                // cap, so the manager will never feed us more than fits.
                let _ = subscribers.push(sub.address);
            })
            .await;

        if subscribers.is_empty() {
            tracing::trace!(
                "No subscribers for service 0x{:04X}, instance {}, event group 0x{:04X}",
                service_id,
                instance_id,
                event_group_id
            );
            return Ok(0);
        }

        // Fail fast with the capacity error rather than letting
        // `encode_to_slice` report a less-actionable protocol I/O error
        // when it runs out of buffer. Matches the raw-event path below
        // and the client socket_manager path.
        let required_size = message.required_size();
        if required_size > UDP_BUFFER_SIZE {
            tracing::error!(
                "Message size ({} bytes) exceeds UDP_BUFFER_SIZE ({}); dropping publish",
                required_size,
                UDP_BUFFER_SIZE
            );
            return Err(Error::Capacity("udp_buffer"));
        }

        // Serialize the message into a fixed-size buffer of
        // `UDP_BUFFER_SIZE` bytes. (In this `async fn` the buffer lives
        // in the future state, not literally on the stack; "MTU-sized"
        // is a misleading description since the cap is a UDP payload
        // limit, not an Ethernet MTU — see `UDP_BUFFER_SIZE` docs.)
        let mut buffer = [0u8; UDP_BUFFER_SIZE];
        let mut message_length = message.encode_to_slice(&mut buffer)?;

        // Apply E2E protect if configured. The `protected` stack buffer is
        // disjoint from `buffer`, so we can read the unprotected payload
        // directly out of `buffer[16..]` without a separate copy.
        {
            let key = E2EKey::from_message_id(message.header().message_id());
            if self.e2e_registry.contains_key(&key) {
                let upper_header: [u8; 8] = buffer[8..16].try_into().expect("upper header slice");
                let mut protected = [0u8; UDP_BUFFER_SIZE];
                let result = self.e2e_registry.protect(
                    key,
                    &buffer[16..message_length],
                    upper_header,
                    &mut protected,
                );
                match result {
                    Some(Ok(protected_len)) => {
                        if 16 + protected_len > UDP_BUFFER_SIZE {
                            tracing::error!(
                                "E2E-protected datagram ({} bytes, header + protected payload) \
                                 exceeds UDP_BUFFER_SIZE ({}); dropping publish",
                                16 + protected_len,
                                UDP_BUFFER_SIZE
                            );
                            return Err(Error::Capacity("udp_buffer"));
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        let new_length: u32 = 8 + protected_len as u32;
                        buffer[4..8].copy_from_slice(&new_length.to_be_bytes());
                        buffer[16..16 + protected_len].copy_from_slice(&protected[..protected_len]);
                        message_length = 16 + protected_len;
                    }
                    Some(Err(e)) => {
                        // Surface protect failures as `Err(Error::E2e(_))`
                        // rather than logging-and-falling-through, which
                        // would silently send the UNPROTECTED payload
                        // claiming an E2E-protected channel and break the
                        // receiver's CRC/counter checks. Counter
                        // exhaustion, key-lookup races, and similar
                        // backend errors all funnel here.
                        tracing::error!("E2E protect error: {:?}; dropping publish", e);
                        return Err(Error::E2e(e));
                    }
                    None => unreachable!("contains_key was true"),
                }
            }
        }

        let datagram = &buffer[..message_length];

        // Send to all snapshotted subscribers. Track the last
        // transport error so we can surface "every send failed" as
        // `Err(Transport(_))` rather than masking total failure as
        // `Ok(0)` — which would be indistinguishable from "no
        // subscribers" to the caller.
        let mut sent_count = 0usize;
        let mut last_err: Option<crate::transport::TransportError> = None;
        for addr in &subscribers {
            match self.socket.get().send_to(datagram, *addr).await {
                Ok(()) => {
                    sent_count += 1;
                    tracing::trace!(
                        "Sent event to subscriber {} ({} bytes)",
                        addr,
                        message_length
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to send event to subscriber {}: {:?}", addr, e);
                    last_err = Some(e);
                }
            }
        }

        tracing::debug!(
            "Published event to {}/{} subscribers for service 0x{:04X}",
            sent_count,
            subscribers.len(),
            service_id
        );

        if sent_count == 0 {
            // Every send failed (subscribers was non-empty above, so
            // last_err is necessarily Some). Surface the most recent
            // transport error so the caller can react.
            return Err(Error::Transport(
                last_err.unwrap_or(crate::transport::TransportError::Unsupported),
            ));
        }
        Ok(sent_count)
    }

    /// Publish raw event data (already serialized with E2E protection)
    ///
    /// This is useful when you've already applied E2E protection to the payload
    ///
    /// # Errors
    ///
    /// Returns an error if the SOME/IP header fails to serialize.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_raw_event(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        event_id: u16,
        request_id: u32,
        protocol_version: u8,
        interface_version: u8,
        payload: &[u8],
    ) -> Result<usize, Error> {
        // Snapshot subscriber addresses into a stack buffer (see
        // publish_event for rationale).
        let mut subscribers: HeaplessVec<SocketAddrV4, SUBSCRIBERS_PER_GROUP> = HeaplessVec::new();
        let _total = self
            .subscriptions
            .for_each_subscriber(service_id, instance_id, event_group_id, |sub| {
                let _ = subscribers.push(sub.address);
            })
            .await;

        if subscribers.is_empty() {
            return Ok(0);
        }

        // Pre-build size check. Fail fast with `Error::Capacity` BEFORE
        // calling `Header::new_event`, which `assert!`s on payloads
        // larger than `u32::MAX as usize - 8`. The earlier
        // `checked_add(header_len, payload.len())` guard below was dead
        // for that reason; keeping it for defence-in-depth on platforms
        // where `Header::SIZE + payload` could overflow `usize`. The
        // `16` here is the SOME/IP header size in bytes.
        if payload.len() > UDP_BUFFER_SIZE.saturating_sub(16) {
            tracing::error!(
                "raw event payload ({} bytes) + 16-byte header exceeds UDP_BUFFER_SIZE ({}); dropping publish",
                payload.len(),
                UDP_BUFFER_SIZE
            );
            return Err(Error::Capacity("udp_buffer"));
        }

        // Build SOME/IP header
        let header = Header::new_event(
            service_id,
            event_id,
            request_id,
            protocol_version,
            interface_version,
            payload.len(),
        );

        // Serialize header + payload into a fixed-size buffer of
        // `UDP_BUFFER_SIZE` bytes. See note in `publish_event` above.
        let mut buffer = [0u8; UDP_BUFFER_SIZE];
        let header_len = header.encode_to_slice(&mut buffer)?;
        let Some(total_len) = header_len.checked_add(payload.len()) else {
            tracing::error!(
                "raw event length computation overflowed usize (header_len={}, payload.len()={}); dropping publish",
                header_len,
                payload.len()
            );
            return Err(Error::Capacity("udp_buffer"));
        };
        // Defence-in-depth: the pre-build guard above already rejects
        // oversize payloads, but a future caller adding optional
        // post-encode tail bytes (e.g. another protect profile) would
        // need this branch. Cheap to keep.
        if total_len > UDP_BUFFER_SIZE {
            tracing::error!(
                "raw event ({} bytes) exceeds UDP_BUFFER_SIZE ({}); dropping publish",
                total_len,
                UDP_BUFFER_SIZE
            );
            return Err(Error::Capacity("udp_buffer"));
        }
        buffer[header_len..total_len].copy_from_slice(payload);
        let datagram = &buffer[..total_len];

        // Send to all snapshotted subscribers; surface total-failure
        // as `Err(Transport(_))` rather than `Ok(0)` (see
        // `publish_event`).
        let mut sent_count = 0usize;
        let mut last_err: Option<crate::transport::TransportError> = None;
        for addr in &subscribers {
            match self.socket.get().send_to(datagram, *addr).await {
                Ok(()) => {
                    sent_count += 1;
                }
                Err(e) => {
                    tracing::error!("Failed to send raw event to {}: {:?}", addr, e);
                    last_err = Some(e);
                }
            }
        }

        if sent_count == 0 {
            return Err(Error::Transport(
                last_err.unwrap_or(crate::transport::TransportError::Unsupported),
            ));
        }
        Ok(sent_count)
    }

    /// Check if there are any active subscribers for a specific event group
    ///
    /// # Arguments
    /// * `service_id` - Service ID
    /// * `instance_id` - Instance ID
    /// * `event_group_id` - Event group ID
    ///
    /// # Returns
    /// `true` if there are subscribers, `false` otherwise
    pub async fn has_subscribers(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> bool {
        self.subscriptions
            .for_each_subscriber(service_id, instance_id, event_group_id, |_| {})
            .await
            > 0
    }

    /// Register a subscriber for an event group.
    ///
    /// This is useful when subscription handling is managed externally
    /// (e.g. by a client that shares the SD socket) rather than by the
    /// server's own `run()` loop.
    ///
    /// Calling this method with the same `(service_id, instance_id,
    /// event_group_id, subscriber_addr)` tuple is idempotent — the
    /// underlying [`super::SubscriptionManager`] deduplicates — so external
    /// dispatchers can safely call it on every incoming
    /// `SubscribeEventGroup` (including TTL refreshes) without growing
    /// the subscriber list.
    ///
    /// # TTL / expiration
    ///
    /// This method does **not** track the SOME/IP-SD Subscribe TTL.
    /// Subscribers registered here persist until explicitly removed via
    /// [`EventPublisher::remove_subscriber`] (or until the
    /// [`EventPublisher`] itself is dropped). External dispatchers are
    /// responsible for detecting stale subscriptions — for example, by
    /// tracking the last refresh time per subscriber and calling
    /// `remove_subscriber` when no refresh has arrived within the
    /// advertised TTL — otherwise subscribers accumulate for the
    /// lifetime of the process.
    ///
    /// # Errors
    ///
    /// Returns [`crate::server::SubscribeError`] when the underlying
    /// [`super::SubscriptionManager`] cannot record the subscription because a
    /// bounded capacity was hit:
    /// - `SubscribersPerGroupFull` — the per-event-group subscriber list
    ///   is full.
    /// - `EventGroupsFull` — the outer event-group map is full.
    ///
    /// On `Err`, the subscriber was **not** registered and no events
    /// will be delivered to `subscriber_addr` for this event group.
    /// External dispatchers should treat this the same way the server's
    /// own `run()` loop does: emit a `SubscribeNack` (or equivalent
    /// upstream notification) so the peer does not assume it is
    /// subscribed. A duplicate registration for an already-subscribed
    /// address returns `Ok(())` (deduplicated).
    pub async fn register_subscriber(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: core::net::SocketAddrV4,
    ) -> Result<(), crate::server::SubscribeError> {
        self.subscriptions
            .subscribe(service_id, instance_id, event_group_id, subscriber_addr)
            .await
    }

    /// Remove a previously-registered subscriber from an event group.
    ///
    /// Counterpart to [`EventPublisher::register_subscriber`] for
    /// externally managed subscriptions. Calling this method with an
    /// address that is not currently subscribed is a no-op.
    ///
    /// Intended for use by external SD dispatchers to clean up stale
    /// subscriptions whose TTL has expired or whose remote peer has
    /// rebooted. The server's own `run()` loop does not call this
    /// method; it is purely for external management.
    pub async fn remove_subscriber(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: core::net::SocketAddrV4,
    ) {
        self.subscriptions
            .unsubscribe(service_id, instance_id, event_group_id, subscriber_addr)
            .await;
    }

    /// Get the current number of subscribers for a specific event group
    pub async fn subscriber_count(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> usize {
        self.subscriptions
            .for_each_subscriber(service_id, instance_id, event_group_id, |_| {})
            .await
    }
}

// Phase 20e collapsed `EventPublisherHandle<R, S, H>` /
// `WrappableEventPublisherHandle<R, S, H>` into the unified
// `crate::transport::SharedHandle<EventPublisher<R, S, H, T>>` /
// `WrappableSharedHandle<EventPublisher<R, S, H, T>>` traits. The
// blanket impls there cover both `&'static EventPublisher<...>`
// and `Arc<EventPublisher<...>>`; no dedicated trait survives
// here.

#[cfg(all(test, feature = "server-tokio"))]
mod tests {
    use super::*;
    use crate::e2e::E2ERegistry;
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use crate::server::SubscriptionManager;
    use crate::tokio_transport::TokioSocket;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Mutex;
    use std::vec;
    use std::vec::Vec;
    use tokio::net::UdpSocket;
    use tokio::sync::RwLock;

    /// Type alias bringing the tokio-flavor concrete type parameters back
    /// into scope so tests can spell `TestEventPublisher` without
    /// chasing the four-type-parameter signature on every call site.
    type TestEventPublisher = EventPublisher<
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<SubscriptionManager>>,
        Arc<TokioSocket>,
        TokioSocket,
    >;

    fn test_registry() -> Arc<Mutex<E2ERegistry>> {
        Arc::new(Mutex::new(E2ERegistry::new()))
    }

    /// Bind a `TokioSocket` for tests. The publisher path under
    /// `server-tokio` already depends on `tokio_transport`, so we use it
    /// directly rather than constructing a `tokio::net::UdpSocket` and
    /// adapting it.
    async fn bind_tokio_socket() -> Arc<TokioSocket> {
        use crate::transport::{SocketOptions, TransportFactory};
        let factory = crate::tokio_transport::TokioTransport;
        Arc::new(
            factory
                .bind(
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
                    &SocketOptions::new(),
                )
                .await
                .expect("bind tokio socket for test"),
        )
    }

    async fn make_publisher(
        subscriptions: Arc<RwLock<SubscriptionManager>>,
    ) -> (TestEventPublisher, Arc<TokioSocket>) {
        let socket = bind_tokio_socket().await;
        let publisher = EventPublisher::new(subscriptions, Arc::clone(&socket), test_registry());
        (publisher, socket)
    }

    fn make_test_message() -> Message<TestPayload> {
        Message::new_sd(0x0001, &empty_sd_header())
    }

    #[tokio::test]
    async fn test_event_publisher_creation() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let socket = bind_tokio_socket().await;

        let publisher = EventPublisher::new(subscriptions, socket, test_registry());
        assert!(std::mem::size_of_val(&publisher) > 0);
    }

    #[tokio::test]
    async fn test_publish_event_no_subscribers() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(subscriptions).await;
        let msg = make_test_message();
        let count = publisher.publish_event(0x5B, 1, 0x01, &msg).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_publish_event_with_subscriber() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));

        // Create a receiver socket to act as subscriber
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let core::net::SocketAddr::V4(recv_addr) = receiver.local_addr().unwrap() else {
            panic!("expected v4 source address");
        };

        // Add subscriber
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, recv_addr).unwrap();
        }

        let (publisher, _) = make_publisher(subscriptions).await;
        let msg = make_test_message();
        let count = publisher.publish_event(0x5B, 1, 0x01, &msg).await.unwrap();
        assert_eq!(count, 1);

        // Verify data was received
        let mut buf = [0u8; 1024];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("timeout receiving event")
        .unwrap();
        assert!(len > 0);
    }

    #[tokio::test]
    async fn test_publish_raw_event_no_subscribers() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(subscriptions).await;
        let count = publisher
            .publish_raw_event(0x5B, 1, 0x01, 0x8001, 0x0001, 0x01, 0x01, &[0xAA, 0xBB])
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_publish_raw_event_exceeds_udp_buffer_returns_capacity_error() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr).unwrap();
        }
        let (publisher, _) = make_publisher(subscriptions).await;

        // Payload = UDP_BUFFER_SIZE forces total (header + payload) over the cap.
        let too_big = vec![0u8; UDP_BUFFER_SIZE];
        let err = publisher
            .publish_raw_event(0x5B, 1, 0x01, 0x8001, 0x0001, 0x01, 0x01, &too_big)
            .await
            .expect_err("oversize payload must error, not report Ok(0)");
        match err {
            Error::Capacity(tag) => assert_eq!(tag, "udp_buffer"),
            other => panic!("expected Error::Capacity(\"udp_buffer\"), got {other:?}"),
        }
    }

    /// Regression for H12: when there ARE subscribers but every
    /// `send_to` fails, `publish_event` must surface the underlying
    /// transport error instead of masking the failure as `Ok(0)` —
    /// which is indistinguishable from "no subscribers" to the caller.
    ///
    /// Uses a mock `TransportSocket` whose `send_to` always returns
    /// `Err(TransportError::Io(IoErrorKind::NetworkUnreachable))`.
    #[tokio::test]
    async fn publish_event_returns_err_when_every_send_fails() {
        use crate::transport::{IoErrorKind, ReceivedDatagram, TransportError, TransportSocket};
        use core::future::{Future, Ready, ready};
        use core::pin::Pin;
        use core::task::{Context, Poll};

        struct AlwaysFailSocket;

        struct AlwaysFailSend;
        impl Future for AlwaysFailSend {
            type Output = Result<(), TransportError>;
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(Err(TransportError::Io(IoErrorKind::NetworkUnreachable)))
            }
        }

        impl TransportSocket for AlwaysFailSocket {
            type SendFuture<'a> = AlwaysFailSend;
            type RecvFuture<'a> = Ready<Result<ReceivedDatagram, TransportError>>;

            fn send_to<'a>(&'a self, _buf: &'a [u8], _t: SocketAddrV4) -> Self::SendFuture<'a> {
                AlwaysFailSend
            }
            fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
                ready(Err(TransportError::Unsupported))
            }
            fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
                Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            }
            fn join_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
                Ok(())
            }
            fn leave_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
                Ok(())
            }
        }

        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr).unwrap();
        }
        #[allow(
            clippy::type_complexity,
            reason = "tests reasonably spell out the full type for clarity"
        )]
        let publisher: EventPublisher<
            Arc<Mutex<E2ERegistry>>,
            Arc<RwLock<SubscriptionManager>>,
            Arc<AlwaysFailSocket>,
            AlwaysFailSocket,
        > = EventPublisher::new(subscriptions, Arc::new(AlwaysFailSocket), test_registry());

        let msg = make_test_message();
        let err = publisher
            .publish_event(0x5B, 1, 0x01, &msg)
            .await
            .expect_err("total-failure path must surface Err, not Ok(0)");
        match err {
            Error::Transport(TransportError::Io(IoErrorKind::NetworkUnreachable)) => {}
            other => panic!(
                "expected Transport(Io(NetworkUnreachable)) from total-failure send, got {other:?}"
            ),
        }
    }

    /// Same H12 path through `publish_raw_event`.
    #[tokio::test]
    async fn publish_raw_event_returns_err_when_every_send_fails() {
        use crate::transport::{IoErrorKind, ReceivedDatagram, TransportError, TransportSocket};
        use core::future::{Future, Ready, ready};
        use core::pin::Pin;
        use core::task::{Context, Poll};

        struct AlwaysFailSocket;
        struct AlwaysFailSend;
        impl Future for AlwaysFailSend {
            type Output = Result<(), TransportError>;
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(Err(TransportError::Io(IoErrorKind::ConnectionRefused)))
            }
        }
        impl TransportSocket for AlwaysFailSocket {
            type SendFuture<'a> = AlwaysFailSend;
            type RecvFuture<'a> = Ready<Result<ReceivedDatagram, TransportError>>;
            fn send_to<'a>(&'a self, _buf: &'a [u8], _t: SocketAddrV4) -> Self::SendFuture<'a> {
                AlwaysFailSend
            }
            fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
                ready(Err(TransportError::Unsupported))
            }
            fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
                Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            }
            fn join_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
                Ok(())
            }
            fn leave_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
                Ok(())
            }
        }

        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr).unwrap();
        }
        #[allow(
            clippy::type_complexity,
            reason = "tests reasonably spell out the full type for clarity"
        )]
        let publisher: EventPublisher<
            Arc<Mutex<E2ERegistry>>,
            Arc<RwLock<SubscriptionManager>>,
            Arc<AlwaysFailSocket>,
            AlwaysFailSocket,
        > = EventPublisher::new(subscriptions, Arc::new(AlwaysFailSocket), test_registry());

        let err = publisher
            .publish_raw_event(0x5B, 1, 0x01, 0x8001, 0x0001, 0x01, 0x01, &[0xAA, 0xBB])
            .await
            .expect_err("total-failure path must surface Err, not Ok(0)");
        match err {
            Error::Transport(TransportError::Io(IoErrorKind::ConnectionRefused)) => {}
            other => panic!("expected Transport(Io(ConnectionRefused)), got {other:?}"),
        }
    }

    /// Regression guard against 343da67: without the pre-check, an oversize
    /// message would fail with a less-actionable protocol I/O error from
    /// `encode_to_slice`'s slice writer running out of buffer, rather than
    /// the explicit `Error::Capacity("udp_buffer")` the new branch returns.
    ///
    /// Note: a subscriber must be registered first — the pre-check sits
    /// after the `subscribers.is_empty()` early return, so without one the
    /// function would return `Ok(0)` and never touch the new branch,
    /// giving a false positive.
    #[tokio::test]
    async fn publish_event_pre_encode_exceeds_udp_buffer_returns_capacity_error() {
        use crate::RawPayload;
        use crate::protocol::{Header, MessageId, MessageType, MessageTypeField, ReturnCode};

        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr).unwrap();
        }
        let (publisher, _) = make_publisher(subscriptions).await;

        // Build a payload that exceeds the UDP cap by one byte based on
        // `UDP_BUFFER_SIZE` instead of a hardcoded fixture length, so the
        // test stays correct if the constant is retuned. Mirrors the
        // client-side oversize fixture in
        // `send_raw_message_exceeding_udp_buffer_returns_capacity_error`.
        let message_id = MessageId::new_from_service_and_method(0x1234, 0x5678);
        let payload_len = UDP_BUFFER_SIZE - 16 + 1  /* SOME/IP header is 16 bytes */;
        let payload_bytes = vec![0u8; payload_len];
        let payload = RawPayload::from_payload_bytes(message_id, &payload_bytes).unwrap();
        let header = Header::new(
            message_id,
            0x0001_0001,
            0x01,
            0x01,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            payload_bytes.len(),
        );
        let message = Message::new(header, payload);
        assert!(
            message.required_size() > UDP_BUFFER_SIZE,
            "fixture must exceed cap",
        );

        let err = publisher
            .publish_event(0x5B, 1, 0x01, &message)
            .await
            .expect_err("oversize message must error, not report Ok(_)");
        match err {
            Error::Capacity(tag) => assert_eq!(tag, "udp_buffer"),
            other => panic!("expected Error::Capacity(\"udp_buffer\"), got {other:?}"),
        }
    }

    /// Messages whose raw encoded size fits `UDP_BUFFER_SIZE` but whose
    /// E2E-protected size does not must be rejected with
    /// `Error::Capacity("udp_buffer")` — guarding the post-protect branch
    /// added alongside the raw-size pre-check.
    #[tokio::test]
    async fn test_publish_event_e2e_protected_exceeds_udp_buffer_returns_capacity_error() {
        use crate::RawPayload;
        use crate::e2e::{E2EProfile, Profile4Config};
        use crate::protocol::MessageId;

        // Register an E2E profile so the protect branch actually runs.
        let message_id = MessageId::new_from_service_and_method(0x5B, 0x8001);
        let key = E2EKey::from_message_id(message_id);
        let mut reg = E2ERegistry::new();
        reg.register(key, E2EProfile::Profile4(Profile4Config::new(0, 15)))
            .expect("E2E registry has capacity for one entry");
        let e2e_registry = Arc::new(Mutex::new(reg));

        // Pre-register a subscriber so we don't short-circuit on the
        // "no subscribers" branch before reaching the E2E guard.
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999))
                .unwrap();
        }

        let socket = bind_tokio_socket().await;
        let publisher = EventPublisher::new(subscriptions, socket, e2e_registry);

        // Size the payload from `UDP_BUFFER_SIZE` and `PROFILE4_HEADER_SIZE`
        // so the raw message fits exactly within the cap — leaving Profile4
        // protection to push the encoded message over the limit and
        // exercise the post-protect guard — regardless of how
        // `UDP_BUFFER_SIZE` is retuned.
        let payload_len = UDP_BUFFER_SIZE - 16; // raw total == UDP_BUFFER_SIZE; SOME/IP header = 16
        let payload_bytes = vec![0u8; payload_len];
        let payload = RawPayload::from_payload_bytes(message_id, &payload_bytes).unwrap();
        let header = Header::new_event(
            message_id.service_id(),
            message_id.method_id(),
            0x0001_0001,
            0x01,
            0x01,
            payload_bytes.len(),
        );
        let message = Message::new(header, payload);
        assert!(
            message.required_size() <= UDP_BUFFER_SIZE,
            "fixture's raw size must fit the cap so the pre-encode check passes and \
             we actually exercise the post-protect guard",
        );

        let err = publisher
            .publish_event(0x5B, 1, 0x01, &message)
            .await
            .expect_err("E2E-protected oversize message must error, not report Ok(n)");
        match err {
            Error::Capacity(tag) => assert_eq!(tag, "udp_buffer"),
            other => panic!("expected Error::Capacity(\"udp_buffer\"), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_publish_raw_event_with_subscriber() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let core::net::SocketAddr::V4(recv_addr) = receiver.local_addr().unwrap() else {
            panic!("expected v4 source address");
        };

        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, recv_addr).unwrap();
        }

        let (publisher, _) = make_publisher(subscriptions).await;
        let payload = [0xDE, 0xAD];
        let count = publisher
            .publish_raw_event(0x5B, 1, 0x01, 0x8001, 0x0001, 0x01, 0x01, &payload)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Verify the received data contains a valid SOME/IP header + payload
        let mut buf = [0u8; 1024];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("timeout receiving raw event")
        .unwrap();
        // 16 bytes header + 2 bytes payload
        assert_eq!(len, 18);
        // Check payload at end
        assert_eq!(&buf[16..18], &payload);
    }

    #[tokio::test]
    async fn test_subscriber_count() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr1 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9001);
        let addr2 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9002);

        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr1).unwrap();
            mgr.subscribe(0x5B, 1, 0x01, addr2).unwrap();
        }

        let (publisher, _) = make_publisher(subscriptions).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 2);
    }

    #[tokio::test]
    async fn test_has_subscribers() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);

        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9001))
                .unwrap();
        }

        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);
    }

    // ── register_subscriber / remove_subscriber ──────────────────────────
    //
    // These cover the externally-managed subscription path used by
    // clients that drive SD through their own discovery socket and
    // dispatch `SubscribeEventGroup` messages into an `EventPublisher`.

    const ADDR_A: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9001);
    const ADDR_B: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9002);
    const ADDR_C: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9003);

    #[tokio::test]
    async fn register_subscriber_adds_to_manager() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }

    #[tokio::test]
    async fn register_subscriber_is_idempotent_on_repeat() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        // Simulate TTL refreshes — the same (tuple, addr) called repeatedly
        // must not grow the subscriber list.
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();

        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }

    #[tokio::test]
    async fn register_subscriber_separates_different_eventgroups() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        publisher
            .register_subscriber(0x5B, 1, 0x02, ADDR_A)
            .await
            .unwrap();

        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x02).await, 1);
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);
        assert!(publisher.has_subscribers(0x5B, 1, 0x02).await);
    }

    #[tokio::test]
    async fn remove_subscriber_happy_path() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 0);
    }

    #[tokio::test]
    async fn remove_subscriber_leaves_siblings_alone() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_B)
            .await
            .unwrap();
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_C)
            .await
            .unwrap();
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 3);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_B).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 2);

        // The remaining two are still in the list.
        let mgr = subscriptions.read().await;
        let subscribers = mgr.get_subscribers(0x5B, 1, 0x01);
        let addrs: Vec<_> = subscribers.iter().map(|s| s.address).collect();
        assert!(addrs.contains(&ADDR_A));
        assert!(addrs.contains(&ADDR_C));
        assert!(!addrs.contains(&ADDR_B));
    }

    #[tokio::test]
    async fn remove_subscriber_nonexistent_is_noop() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        // Remove from an empty manager.
        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 0);

        // Register one subscriber, then remove a different address.
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_B).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);

        // Remove with wrong service_id is also a no-op.
        publisher.remove_subscriber(0x99, 1, 0x01, ADDR_A).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }

    #[tokio::test]
    async fn remove_subscriber_all_then_has_subscribers_false() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_B)
            .await
            .unwrap();
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_B).await;
        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);
    }

    #[tokio::test]
    async fn register_and_remove_roundtrip_preserves_idempotence() {
        // Register → remove → register again should end with exactly one
        // subscriber; the remove in the middle should not leave ghost state.
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();
        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        publisher
            .register_subscriber(0x5B, 1, 0x01, ADDR_A)
            .await
            .unwrap();

        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }
}
