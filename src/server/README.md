# SOME/IP Server Implementation

This directory contains the server/provider functionality for `simple_someip`, allowing you to **offer services** and **publish events** rather than just consuming them.

## Overview

The server implementation provides:

1. **Service Announcement** - Periodically broadcast OfferService messages via Service Discovery (SD)
2. **Event Publishing** - Send events to subscribed clients
3. **Subscription Management** - Track who's subscribed to which event groups
4. **UDP Socket Management** - Handle unicast and multicast communication

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
│ 224.244.224.245│      │   Subscriber   │      │   Subscriber   │
│    :30490      │      │   (unicast)    │      │   (unicast)    │
│  (multicast)   │      └────────────────┘      └────────────────┘
└────────────────┘
```

## Usage Example

### Basic Server Setup

```rust
use simple_someip::server::{Server, ServerConfig};
use std::net::Ipv4Addr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure the server
    let config = ServerConfig::new(
        Ipv4Addr::new(192, 168, 1, 200), // Local interface
        30500,                            // Local port
        0x1234,                           // Service ID
        1,                                // Instance ID
    );

    // Create the server
    let mut server = Server::new(config).await?;

    // Start announcing the service (sends OfferService every 1s)
    server.start_announcing().await?;

    // Get event publisher for sending events
    let publisher = server.publisher();

    // Spawn the server run loop to handle subscriptions
    tokio::spawn(async move {
        if let Err(e) = server.run().await {
            eprintln!("Server error: {:?}", e);
        }
    });

    // Publish events to subscribers
    publisher.publish_raw_event(
        0x1234,    // service_id
        1,         // instance_id
        0x01,      // event_group_id
        0x8001,    // event_id
        0,         // session_id
        1,         // protocol_version
        1,         // interface_version
        &[0x01],   // payload
    ).await?;

    Ok(())
}
```

## Service Discovery (SD) Protocol

The server periodically sends **OfferService** messages to the multicast group `224.244.224.245:30490`:

```
SD Message Structure:
├─ Flags: Reboot=true, Unicast=false
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

Configuration for a SOME/IP service provider:

- `interface: Ipv4Addr` - Local network interface
- `local_port: u16` - Port to bind for receiving subscriptions
- `service_id: u16` - SOME/IP service ID
- `instance_id: u16` - Service instance ID
- `major_version: u8` - Service major version (default: 1)
- `minor_version: u32` - Service minor version (default: 0)
- `ttl: u32` - Service Discovery TTL in seconds (default: 3)

### `Server`

Main server struct:

- `new(config: ServerConfig) -> Result<Self>` - Create new server
- `start_announcing() -> Result<()>` - Start SD announcements
- `publisher() -> Arc<EventPublisher>` - Get event publisher
- `run() -> Result<()>` - Run event loop (handles subscriptions)

### `EventPublisher`

Publishes events to subscribers:

- `publish_raw_event(service_id, instance_id, event_group_id, event_id, session_id, protocol_version, interface_version, payload) -> Result<usize>`
  - Sends event to all subscribers
  - Returns number of subscribers that received the event
- `has_subscribers(service_id, instance_id, event_group_id) -> bool`
  - Check if any subscribers exist for an event group

### `SubscriptionManager`

Manages event group subscriptions:

- `subscribe(service_id, instance_id, event_group_id, subscriber_addr)` - Add subscriber (deduplicates automatically)
- `unsubscribe(service_id, instance_id, event_group_id, subscriber_addr)` - Remove subscriber
- `get_subscribers(service_id, instance_id, event_group_id) -> Vec<Subscriber>` - Get all subscribers

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
