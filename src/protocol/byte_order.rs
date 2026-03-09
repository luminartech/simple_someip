use crate::protocol::Error;
use embedded_io::Error as _;

/// Extension trait for writing big-endian integers to a byte stream.
///
/// The only required method is [`write_bytes`](WriteBytesExt::write_bytes).
/// Backed by `embedded_io::Write` via a blanket impl.
pub trait WriteBytesExt {
    /// Write all bytes from `buf` to the stream.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_bytes(&mut self, buf: &[u8]) -> Result<(), Error>;

    /// Write a single byte to the stream.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u8(&mut self, val: u8) -> Result<(), Error> {
        self.write_bytes(&[val])
    }

    /// Write a `u16` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u16_be(&mut self, val: u16) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write the lower 3 bytes of a `u32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u24_be(&mut self, val: u32) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes()[1..])
    }

    /// Write a `u32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u32_be(&mut self, val: u32) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }
}

impl<T: embedded_io::Write> WriteBytesExt for T {
    fn write_bytes(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.write_all(buf).map_err(|e| Error::Io(e.kind()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter;

    impl embedded_io::ErrorType for FailingWriter {
        type Error = embedded_io::ErrorKind;
    }

    impl embedded_io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> Result<usize, Self::Error> {
            Err(embedded_io::ErrorKind::BrokenPipe)
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    // --- WriteBytesExt error mapping ---

    #[test]
    fn write_io_error_maps_to_error_io() {
        assert!(matches!(
            FailingWriter.write_u8(0),
            Err(Error::Io(embedded_io::ErrorKind::BrokenPipe))
        ));
    }

    // --- WriteBytesExt happy path ---

    #[test]
    fn write_u8_encodes_correctly() {
        let mut buf = [0u8; 1];
        buf.as_mut_slice().write_u8(0xAB).unwrap();
        assert_eq!(buf, [0xAB]);
    }

    #[test]
    fn write_u16_be_encodes_correctly() {
        let mut buf = [0u8; 2];
        buf.as_mut_slice().write_u16_be(0x0102).unwrap();
        assert_eq!(buf, [0x01, 0x02]);
    }

    #[test]
    fn write_u24_be_encodes_correctly() {
        let mut buf = [0u8; 3];
        buf.as_mut_slice().write_u24_be(0x0001_0203).unwrap();
        assert_eq!(buf, [0x01, 0x02, 0x03]);
    }

    #[test]
    fn write_u32_be_encodes_correctly() {
        let mut buf = [0u8; 4];
        buf.as_mut_slice().write_u32_be(0x0102_0304).unwrap();
        assert_eq!(buf, [0x01, 0x02, 0x03, 0x04]);
    }
}
