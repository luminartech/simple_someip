# Simple SOME/IP

Simple SOME/IP intends to make working with basic, IpV4 SOME/IP entities as easy as possible.
It supports basic Service Discovery, events, anad methods with mininimal integration.
The crate is focused on usability and ergonomics for tooling and scripting uses rather than use in an embedded context.
The library is based on Tokio and utilizes std::io traits for serialization and deserialization.

## Organization

The crate is organized into several modules:
- `client`: Provides a high-level client for interacting with SOME/IP services.
- `protocol`: Contains definitions for sending and parsing SOME/IP protocol messages.
- `error`: Defines error types used throughout the crate.
- `traits`: Contains traits for defining SOME/IP services, methods, allowing the library automatically handle serialization and deserialization of custom types.


## Usage

To use Simple SOME/IP in your project, add the following to your `Cargo.toml`:

```toml
[dependencies]
simple_someip = "0.2"
```

Then, you can create a client and interact with SOME/IP services as demonstrated in the examples provided in the repository.
