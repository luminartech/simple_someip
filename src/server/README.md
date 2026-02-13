# SOME/IP Server Implementation

This directory contains the server/provider functionality for `simple_someip`, allowing you to **offer services** and **publish events** rather than just consuming them.

## Overview

The server implementation provides:

1. **Service Announcement** - Periodically broadcast OfferService messages via Service Discovery (SD)
2. **Event Publishing** - Send events to subscribed clients with E2E protection
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
        30682,                            // Local port
        0x5B,                             // Service ID (SystemModeCommand)
        1,                                // Instance ID
    );

    // Create the server
    let server = Server::new(config).await?;
    
    // Start announcing the service (sends OfferService every 1s)
    server.start_announcing().await?;
    
    // Get event publisher for sending events
    let publisher = server.publisher();
    
    Ok(())
}
```

### Publishing Events with E2E Protection

```rust
use iris_someip::e2e::{protect_system_mode_ctrl, IrisE2EConfigs, IrisE2EStates};
use iris_someip::messages::SystemMode;

// Initialize E2E protection
let configs = IrisE2EConfigs::new();
let mut states = IrisE2EStates::new();

// Create payload
let payload = vec![SystemMode::Active as u8];

// Apply E2E protection (adds CRC + counter)
let protected = protect_system_mode_ctrl(&configs, &mut states, &payload);

// Publish to subscribers
publisher.publish_raw_event(
    0x5B,      // service_id
    1,         // instance_id
    0x01,      // event_group_id
    0x8001,    // event_id
    0,         // session_id
    1,         // protocol_version
    1,         // interface_version
    &protected,
).await?;
```

## Complete Example

See `iris_someip/examples/system_mode_server.rs` for a complete working example that:
- Offers the SystemModeCommand service (0x5B)
- Publishes SystemModeCtrl events with E2E protection
- Accepts mode selection via command line
- Logs all activity

### Running the Example

```powershell
# Build the server
cargo build --example system_mode_server

# Run with default settings (Active mode, 1 Hz)
cargo run --example system_mode_server

# Run with custom settings
cargo run --example system_mode_server -- `
    --interface 192.168.1.200 `
    --port 30682 `
    --mode active `
    --interval-ms 1000
```

## Service Discovery (SD) Protocol

The server periodically sends **OfferService** messages to the multicast group `224.244.224.245:30490`:

```
SD Message Structure:
├─ Flags: Reboot=true, Unicast=false
├─ Entry: OfferService
│  ├─ Service ID: 0x5B
│  ├─ Instance ID: 1
│  ├─ Major Version: 1
│  ├─ Minor Version: 0
│  └─ TTL: 3 seconds
└─ Option: IPv4 Endpoint
   ├─ IP: 192.168.1.200
   ├─ Port: 30682
   └─ Protocol: UDP
```

Clients listening on the multicast group will discover your service and can subscribe to event groups.

## Event Publishing

Events are sent as SOME/IP notifications with E2E protection:

```
SOME/IP Message:
├─ Header (16 bytes)
│  ├─ Service ID: 0x5B
│  ├─ Method ID: 0x8001 (event)
│  ├─ Length: payload length + 8
│  ├─ Session ID: incrementing counter
│  ├─ Protocol Version: 1
│  ├─ Interface Version: 1
│  ├─ Message Type: Notification (0x02)
│  └─ Return Code: OK (0x00)
└─ Payload (E2E protected)
   ├─ CRC-16 (2 bytes)
   ├─ Counter (1 byte)
   └─ Data (N bytes)
```

## Testing with the Sensor

To test your server implementation with an actual Iris sensor:

1. **Connect to the sensor's network** (e.g., 192.168.1.x)

2. **Run the server** on your PC:
   ```powershell
   cargo run --example system_mode_server -- --interface 192.168.1.200 --mode active
   ```

3. **Monitor service discovery** with Wireshark:
   - Filter: `udp.port == 30490`
   - Look for OfferService messages from your PC

4. **Watch for subscriptions** from the sensor:
   - The sensor should discover your service
   - It will send Subscribe messages to your unicast port
   - Your server will then publish events to the sensor

5. **Verify the sensor responds**:
   - Check if the sensor transitions to Active mode
   - Look for sensor status messages
   - Monitor point cloud output (if applicable)

## Architecture Notes

### Event-Based vs Request/Response

This implementation follows the **event-based** model where:
- **Server OFFERS services** (announces availability)
- **Clients SUBSCRIBE to event groups**
- **Server PUBLISHES events** to all subscribers

This is different from the traditional request/response model where clients call methods and wait for responses.

### Why This Architecture?

The Iris sensor expects the **client (your PC) to offer SystemModeCommand** as a service. The sensor acts as a subscriber, listening for mode change events. This is common in automotive systems where:
- Control commands are broadcast as events
- Multiple ECUs can subscribe to the same commands
- No request/response overhead
- Better suited for real-time systems

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

### `SubscriptionManager`

Manages event group subscriptions:

- `subscribe(service_id, instance_id, event_group_id, subscriber_addr)` - Add subscriber
- `unsubscribe(service_id, instance_id, event_group_id, subscriber_addr)` - Remove subscriber
- `get_subscribers(service_id, instance_id, event_group_id) -> Vec<Subscriber>` - Get all subscribers

## Troubleshooting

### No subscribers

**Problem**: Server starts but logs "No subscribers (yet)"

**Solution**: 
- Verify sensor can see SD announcements (Wireshark)
- Check firewall settings (allow UDP on your port and 30490)
- Ensure correct network interface selected

### Service not discovered

**Problem**: Sensor doesn't discover your service

**Solution**:
- Verify multicast routing: `route print` (Windows)
- Check interface IP matches sensor's subnet
- Try increasing TTL in ServerConfig
- Monitor with: `tcpdump -i <interface> host 224.244.224.245`

### Events not received by sensor

**Problem**: Server sends events but sensor doesn't respond

**Solution**:
- Verify E2E protection is correct (CRC, counter, data ID)
- Check event ID matches ARXML (0x8001 for SystemModeCtrl)
- Ensure payload structure matches sensor expectations
- Use Wireshark to compare with working vsomeip implementation

## Next Steps

1. **Implement subscription handling** - Currently subscriptions are logged but not processed
2. **Add request/response support** - For method calls in addition to events
3. **Implement TCP transport** - Currently only UDP is supported
4. **Add more complete SD support** - StopOfferService, Subscribe/SubscribeAck handling
5. **Implement event groups** - Group related events together

## License

Same as parent project.
