# SOME/IP Server Implementation

This directory contains the server/provider functionality for `simple_someip`, allowing you to **offer services** and **publish events** rather than just consuming them.

## Overview

The server implementation provides:

1. **Service Announcement** - Periodically broadcast OfferService messages via Service Discovery (SD)
2. **Event Publishing** - Send events to subscribed clients
3. **Subscription Management** - Track who's subscribed to which event groups
4. **End-to-End Protection** - A caller-supplied `E2EHandle` checks every request and protects every event and response
5. **UDP Socket Management** - Handle unicast and multicast communication

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                         Server                                │
├──────────────────────────────────────────────────────────────┤
│  - ServerConfig (service ID, ports, TTL)                     │
│  - Unicast socket (receives subscriptions)                   │
│  - SD socket (sends announcements)                           │
│  - SubscriptionManager (tracks subscribers)                  │
│  - EventPublisher (sends events to subscribers)              │
└──────────────────────────────────────────────────────────────┘
         │                       │                       │
         │ SD Announcements      │ Events                │ Subscriptions
         ↓                       ↓                       ↑
┌────────────────┐      ┌────────────────┐      ┌────────────────┐
│  239.255.0.255 │      │   Subscriber   │      │   Subscriber   │
│    :30490      │      │   (unicast)    │      │   (unicast)    │
│  (multicast)   │      └────────────────┘      └────────────────┘
└────────────────┘
```

## Usage Example

### Basic Server Setup

```rust
use simple_someip::ServiceInstance;
use simple_someip::server::{Offering, Server, ServerConfig};
use std::net::Ipv4Addr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure the offered service and the local interface
    let service = ServiceInstance::new(0x1234, 1, 1).expect("not a wildcard");
    let config = ServerConfig::new(Offering::new(service).with_local_port(30500))
        .with_interface(Ipv4Addr::new(192, 168, 1, 200));

    // Create the server. `run` drives the receive loop and sends an
    // OfferService every 1 s.
    let (_server, handles, run) = Server::new(config).await?;
    let publisher = handles.publisher;
    tokio::spawn(async move {
        if let Err(e) = run.await {
            eprintln!("Server error: {e:?}");
        }
    });

    // Publish events to subscribers using raw bytes
    publisher.publish_raw_event(
        0x1234,    // service_id
        1,         // instance_id
        0x01,      // event_group_id
        0x8001,    // event_id
        0,         // request_id
        1,         // protocol_version
        1,         // interface_version
        &[0x01],   // payload
    ).await?;

    Ok(())
}
```

## Service Discovery (SD) Protocol

The server periodically sends **OfferService** messages to the multicast group `239.255.0.255:30490`:

```
SD Message Structure:
├─ Flags: Reboot=Recently/Continuous (per session-counter wrap), Unicast=true
├─ Entry: OfferService
│  ├─ Service ID
│  ├─ Instance ID
│  ├─ Major Version
│  ├─ Minor Version
│  └─ TTL (seconds)
└─ Option: IPv4 Endpoint
   ├─ IP address
   ├─ Port
   └─ Protocol: UDP
```

Clients listening on the multicast group will discover your service and can subscribe to event groups.

## Event Publishing

Events are sent as SOME/IP notifications:

```
SOME/IP Message:
├─ Header (16 bytes)
│  ├─ Service ID
│  ├─ Method ID (event ID)
│  ├─ Length: payload length + 8
│  ├─ Session ID: incrementing counter
│  ├─ Protocol Version: 1
│  ├─ Interface Version: 1
│  ├─ Message Type: Notification (0x02)
│  └─ Return Code: OK (0x00)
└─ Payload (application-defined)
```

## Architecture Notes

### Event-Based vs Request/Response

This implementation follows the **event-based** model where:
- **Server OFFERS services** (announces availability)
- **Clients SUBSCRIBE to event groups**
- **Server PUBLISHES events** to all subscribers

This is common in automotive SOME/IP deployments where control commands are broadcast as events, allowing multiple ECUs to subscribe without request/response overhead.

## API Reference

### `ServerConfig`

Configuration for a SOME/IP server:

- `interface: Ipv4Addr` - Local network interface
- `offering: Option<Offering>` - The offered service; `None` from `ServerConfig::consumer_only()`
- `consumed_eventgroups` - Eventgroups the server finds and subscribes to
- `required_services` - Service instances the server finds
- `find_timing: FindTiming` - When `FindService` is sent
- `sd_multicast: SocketAddrV4` - SD multicast group and port (default: `239.255.0.255:30490`)

### `Offering`

The offered service:

- `service: ServiceInstance` - Service ID, instance ID and major version
- `minor_version: u32` - Service minor version (default: 0)
- `ttl: u32` - Service Discovery TTL in seconds (default: 3)
- `event_group_ids` - Event groups accepted in a Subscribe (default: any)
- `local_port: u16` - Port to bind for receiving subscriptions (default: ephemeral)
- `announce: bool` - Whether the server sends `OfferService` itself (default: `true`)
- `accepted_offers` - Co-offered `(service, event_group)` pairs accepted in a Subscribe

### `Server`

Main server struct:

- `new(config: ServerConfig) -> Result<(Server, ServerHandles, impl Future<Output = Result<(), Error>>), Error>` - Create a server; the returned future drives the receive loop and the announcements
- `publisher() -> Hep` - The event publisher, also in `ServerHandles::publisher`

### `EventPublisher`

Publishes events to subscribers:

- `publish_event(service_id, instance_id, event_group_id, message) -> Result<usize>`
  - Type-safe event publishing using `Message<P>`
  - Applies the server's `E2EHandle` protection when it protects the message
  - Returns number of subscribers that received the event
- `publish_raw_event(service_id, instance_id, event_group_id, event_id, session_id, protocol_version, interface_version, payload) -> Result<usize>`
  - Low-level event publishing using raw bytes
  - Applies the server's `E2EHandle` protection when it protects the message
  - Returns number of subscribers that received the event
- `register_subscriber(service_id, instance_id, event_group_id, subscriber_addr) -> Result<(), SubscribeError>`
  - Manually register a subscriber (advanced use; the built-in SD loop calls this for you)
  - Capacity-rejects with `SubscribeError::*` so external dispatchers can emit a `SubscribeNack`
- `remove_subscriber(service_id, instance_id, event_group_id, subscriber_addr)`
  - Manually remove a subscriber
- `has_subscribers(service_id, instance_id, event_group_id) -> bool`
  - Check if any subscribers exist for an event group
- `subscriber_count(service_id, instance_id, event_group_id) -> usize`
  - Returns the number of subscribers for an event group

### `SubscriptionManager`

Manages event group subscriptions:

- `subscribe(service_id, instance_id, event_group_id, subscriber_addr) -> Result<(), SubscribeError>` - Add subscriber (deduplicates automatically); returns `Err` when a fixed-capacity bound (`SUBSCRIBERS_PER_GROUP` or `EVENT_GROUPS_CAP`) is exhausted
- `unsubscribe(service_id, instance_id, event_group_id, subscriber_addr)` - Remove subscriber
- `get_subscribers(service_id, instance_id, event_group_id) -> Vec<Subscriber>` - Get all subscribers

External dispatchers (those calling `EventPublisher::register_subscriber` directly) must NACK on `Err(SubscribeError::*)`; the server's built-in SD loop already does this automatically.

## Troubleshooting

### No subscribers

**Problem**: Server starts but no subscribers appear

**Solution**:
- Verify clients can see SD announcements (check with Wireshark, filter `udp.port == 30490`)
- Check firewall settings (allow UDP on your service port and 30490)
- Ensure the correct network interface is selected

### Service not discovered

**Problem**: Clients don't discover your service

**Solution**:
- Verify multicast routing on your OS
- Check interface IP matches the client's subnet
- Try increasing TTL in ServerConfig
- Monitor with Wireshark for OfferService messages
