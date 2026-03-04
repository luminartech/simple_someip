use crate::protocol::sd;
use crate::traits::{PayloadWireFormat, WireFormat};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TestSdHeader {
    pub flags: sd::Flags,
    pub entries: heapless::Vec<sd::Entry, 4>,
    pub options: heapless::Vec<sd::Options, 4>,
}

impl WireFormat for TestSdHeader {
    fn required_size(&self) -> usize {
        sd::Header::new(self.flags, &self.entries, &self.options).required_size()
    }
    fn encode<T: embedded_io::Write>(
        &self,
        writer: &mut T,
    ) -> Result<usize, crate::protocol::Error> {
        sd::Header::new(self.flags, &self.entries, &self.options).encode(writer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TestPayload {
    pub header: TestSdHeader,
}

impl PayloadWireFormat for TestPayload {
    type SdHeader = TestSdHeader;
    fn message_id(&self) -> crate::protocol::MessageId {
        crate::protocol::MessageId::SD
    }
    fn as_sd_header(&self) -> Option<&TestSdHeader> {
        Some(&self.header)
    }
    fn from_payload_bytes(
        message_id: crate::protocol::MessageId,
        payload: &[u8],
    ) -> Result<Self, crate::protocol::Error> {
        match message_id {
            crate::protocol::MessageId::SD => {
                let view = sd::SdHeaderView::parse(payload)?;
                let mut entries = heapless::Vec::new();
                for ev in view.entries() {
                    entries.push(ev.to_owned().unwrap()).ok();
                }
                let mut options = heapless::Vec::new();
                for ov in view.options() {
                    options.push(ov.to_owned().unwrap()).ok();
                }
                Ok(Self {
                    header: TestSdHeader {
                        flags: view.flags(),
                        entries,
                        options,
                    },
                })
            }
            _ => Err(crate::protocol::Error::UnsupportedMessageID(message_id)),
        }
    }
    fn new_sd_payload(header: &TestSdHeader) -> Self {
        Self {
            header: header.clone(),
        }
    }
    fn sd_flags(&self) -> Option<sd::Flags> {
        Some(self.header.flags)
    }
    fn required_size(&self) -> usize {
        self.header.required_size()
    }
    fn encode<T: embedded_io::Write>(
        &self,
        writer: &mut T,
    ) -> Result<usize, crate::protocol::Error> {
        self.header.encode(writer)
    }
    #[cfg(feature = "std")]
    fn new_subscription_sd_header(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_ip: std::net::Ipv4Addr,
        protocol: sd::TransportProtocol,
        client_port: u16,
    ) -> TestSdHeader {
        let entry = sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
        ));
        let endpoint = sd::Options::IpV4Endpoint {
            ip: client_ip,
            protocol,
            port: client_port,
        };
        let mut entries = heapless::Vec::new();
        entries.push(entry).unwrap();
        let mut options = heapless::Vec::new();
        options.push(endpoint).unwrap();
        TestSdHeader {
            flags: sd::Flags::new_sd(false),
            entries,
            options,
        }
    }
}

pub(crate) fn empty_sd_header() -> TestSdHeader {
    TestSdHeader {
        flags: sd::Flags::new_sd(false),
        entries: heapless::Vec::new(),
        options: heapless::Vec::new(),
    }
}
