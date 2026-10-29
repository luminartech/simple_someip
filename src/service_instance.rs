//! The identity of a SOME/IP service instance.

/// A SOME/IP service instance: a service ID, an instance ID and a major
/// version, none of them a wildcard.
///
/// The wildcards are service ID `0xFFFF`, instance ID `0xFFFF` and major
/// version `0xFF`. A `FindService` entry may use them; an offer, a
/// subscription or a required service names one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServiceInstance {
    service_id: u16,
    instance_id: u16,
    major_version: u8,
}

impl ServiceInstance {
    /// Builds a [`ServiceInstance`]; `None` when any argument is a wildcard.
    ///
    /// # Arguments
    ///
    /// * `service_id` - the SOME/IP service ID, not `0xFFFF`.
    /// * `instance_id` - the SOME/IP instance ID, not `0xFFFF`.
    /// * `major_version` - the major version of the service interface, not
    ///   `0xFF`.
    ///
    /// # Examples
    ///
    /// ```
    /// use simple_someip::ServiceInstance;
    ///
    /// let instance = ServiceInstance::new(0x1234, 1, 1).unwrap();
    /// assert_eq!(instance.service_id(), 0x1234);
    /// assert!(ServiceInstance::new(0xFFFF, 1, 1).is_none());
    /// ```
    #[must_use]
    pub const fn new(service_id: u16, instance_id: u16, major_version: u8) -> Option<Self> {
        if service_id == 0xFFFF || instance_id == 0xFFFF || major_version == 0xFF {
            None
        } else {
            Some(Self {
                service_id,
                instance_id,
                major_version,
            })
        }
    }

    /// The SOME/IP service ID.
    #[must_use]
    pub const fn service_id(self) -> u16 {
        self.service_id
    }

    /// The SOME/IP instance ID.
    #[must_use]
    pub const fn instance_id(self) -> u16 {
        self.instance_id
    }

    /// The major version of the service interface.
    #[must_use]
    pub const fn major_version(self) -> u8 {
        self.major_version
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceInstance;

    #[test]
    fn new_refuses_each_wildcard() {
        assert!(ServiceInstance::new(0xFFFF, 1, 1).is_none());
        assert!(ServiceInstance::new(1, 0xFFFF, 1).is_none());
        assert!(ServiceInstance::new(1, 1, 0xFF).is_none());
    }

    #[test]
    fn new_keeps_each_field() {
        let instance = ServiceInstance::new(0xFFFE, 0xFFFE, 0xFE).unwrap();
        assert_eq!(
            (
                instance.service_id(),
                instance.instance_id(),
                instance.major_version()
            ),
            (0xFFFE, 0xFFFE, 0xFE)
        );
    }
}
