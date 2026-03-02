use crate::protocol::Error;
use embedded_io::Error as _;

/// Extension trait for reading big-endian integers from a byte stream.
///
/// The only required method is [`read_bytes`](ReadBytesExt::read_bytes).
/// Backed by `embedded_io::Read` via a blanket impl.
pub(crate) trait ReadBytesExt {
    /// Read exactly `buf.len()` bytes into `buf`.
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), Error>;

    fn read_u8(&mut self) -> Result<u8, Error> {
        let mut buf = [0u8; 1];
        self.read_bytes(&mut buf)?;
        Ok(buf[0])
    }

    fn read_u16_be(&mut self) -> Result<u16, Error> {
        let mut buf = [0u8; 2];
        self.read_bytes(&mut buf)?;
        Ok(u16::from_be_bytes(buf))
    }

    fn read_u24_be(&mut self) -> Result<u32, Error> {
        let mut buf = [0u8; 3];
        self.read_bytes(&mut buf)?;
        Ok(u32::from_be_bytes([0, buf[0], buf[1], buf[2]]))
    }

    fn read_u32_be(&mut self) -> Result<u32, Error> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf)?;
        Ok(u32::from_be_bytes(buf))
    }
}

impl<T: embedded_io::Read> ReadBytesExt for T {
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.read_exact(buf).map_err(|e| match e {
            embedded_io::ReadExactError::UnexpectedEof => Error::UnexpectedEof,
            embedded_io::ReadExactError::Other(e) => Error::Io(e.kind()),
        })
    }
}

/// Extension trait for writing big-endian integers to a byte stream.
///
/// The only required method is [`write_bytes`](WriteBytesExt::write_bytes).
/// Backed by `embedded_io::Write` via a blanket impl.
pub(crate) trait WriteBytesExt {
    /// Write all bytes from `buf` to the stream.
    fn write_bytes(&mut self, buf: &[u8]) -> Result<(), Error>;

    fn write_u8(&mut self, val: u8) -> Result<(), Error> {
        self.write_bytes(&[val])
    }

    fn write_u16_be(&mut self, val: u16) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    fn write_u24_be(&mut self, val: u32) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes()[1..])
    }

    fn write_u32_be(&mut self, val: u32) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }
}

impl<T: embedded_io::Write> WriteBytesExt for T {
    fn write_bytes(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.write_all(buf).map_err(|e| Error::Io(e.kind()))
    }
}

/// A reader wrapper that limits reads to a specified number of bytes.
/// Equivalent to `std::io::Read::take()` but works with `embedded_io::Read`.
pub(crate) struct Take<'a, R> {
    inner: &'a mut R,
    remaining: usize,
}

impl<'a, R> Take<'a, R> {
    pub fn new(inner: &'a mut R, limit: usize) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: embedded_io::ErrorType> embedded_io::ErrorType for Take<'_, R> {
    type Error = R::Error;
}

impl<R: embedded_io::Read> embedded_io::Read for Take<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let max = buf.len().min(self.remaining);
        let n = self.inner.read(&mut buf[..max])?;
        self.remaining -= n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "std")]
    use std::vec::Vec;

    // Minimal reader/writer stubs that always return an I/O error.
    struct FailingReader;

    impl embedded_io::ErrorType for FailingReader {
        type Error = embedded_io::ErrorKind;
    }

    impl embedded_io::Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Self::Error> {
            Err(embedded_io::ErrorKind::BrokenPipe)
        }
    }

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

    // --- ReadBytesExt error mapping ---

    #[test]
    fn read_truncated_input_returns_unexpected_eof() {
        assert!(matches!((&[][..]).read_u8(), Err(Error::UnexpectedEof)));
    }

    #[test]
    fn read_io_error_maps_to_error_io() {
        assert!(matches!(
            FailingReader.read_u8(),
            Err(Error::Io(embedded_io::ErrorKind::BrokenPipe))
        ));
    }

    // --- ReadBytesExt happy path ---

    #[test]
    fn read_u8_decodes_correctly() {
        assert_eq!((&[0xAB][..]).read_u8().unwrap(), 0xAB);
    }

    #[test]
    fn read_u16_be_decodes_correctly() {
        assert_eq!((&[0x01, 0x02][..]).read_u16_be().unwrap(), 0x0102);
    }

    #[test]
    fn read_u16_be_truncated_returns_unexpected_eof() {
        assert!(matches!(
            (&[0x01][..]).read_u16_be(),
            Err(Error::UnexpectedEof)
        ));
    }

    #[test]
    fn read_u24_be_decodes_correctly() {
        assert_eq!(
            (&[0x01, 0x02, 0x03][..]).read_u24_be().unwrap(),
            0x0001_0203
        );
    }

    #[test]
    fn read_u24_be_truncated_returns_unexpected_eof() {
        assert!(matches!(
            (&[0x01, 0x02][..]).read_u24_be(),
            Err(Error::UnexpectedEof)
        ));
    }

    #[test]
    fn read_u32_be_decodes_correctly() {
        assert_eq!(
            (&[0x01, 0x02, 0x03, 0x04][..]).read_u32_be().unwrap(),
            0x0102_0304
        );
    }

    #[test]
    fn read_u32_be_truncated_returns_unexpected_eof() {
        assert!(matches!(
            (&[0x01, 0x02, 0x03][..]).read_u32_be(),
            Err(Error::UnexpectedEof)
        ));
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

    #[cfg(feature = "std")]
    #[test]
    fn write_u8_encodes_correctly() {
        let mut out = Vec::new();
        out.write_u8(0xAB).unwrap();
        assert_eq!(out, [0xAB]);
    }

    #[cfg(feature = "std")]
    #[test]
    fn write_u16_be_encodes_correctly() {
        let mut out = Vec::new();
        out.write_u16_be(0x0102).unwrap();
        assert_eq!(out, [0x01, 0x02]);
    }

    #[cfg(feature = "std")]
    #[test]
    fn write_u24_be_encodes_correctly() {
        let mut out = Vec::new();
        out.write_u24_be(0x0001_0203).unwrap();
        assert_eq!(out, [0x01, 0x02, 0x03]);
    }

    #[cfg(feature = "std")]
    #[test]
    fn write_u32_be_encodes_correctly() {
        let mut out = Vec::new();
        out.write_u32_be(0x0102_0304).unwrap();
        assert_eq!(out, [0x01, 0x02, 0x03, 0x04]);
    }

    // --- Take ---

    #[test]
    fn take_limits_reads_and_signals_eof_at_limit() {
        let data: &[u8] = &[0x01, 0x02, 0x03, 0x04];
        let mut reader = data;
        let mut taken = Take::new(&mut reader, 2);
        assert_eq!(taken.read_u16_be().unwrap(), 0x0102);
        assert!(matches!(taken.read_u8(), Err(Error::UnexpectedEof)));
    }
}
