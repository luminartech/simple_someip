use crate::protocol::Error;
use embedded_io::Error as _;

/// Extension trait for reading big-endian values from a byte stream.
///
/// The only required method is [`read_bytes`](ReadBytesExt::read_bytes).
/// Backed by `embedded_io::Read` via a blanket impl.
pub trait ReadBytesExt {
    /// Read exactly `buf.len()` bytes from the stream.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), Error>;

    /// Read a single `u8`.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_u8(&mut self) -> Result<u8, Error> {
        let mut buf = [0u8; 1];
        self.read_bytes(&mut buf)?;
        Ok(buf[0])
    }

    /// Read an `i8`.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_i8(&mut self) -> Result<i8, Error> {
        self.read_u8().map(u8::cast_signed)
    }

    /// Read a `u16` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_u16_be(&mut self) -> Result<u16, Error> {
        let mut buf = [0u8; 2];
        self.read_bytes(&mut buf)?;
        Ok(u16::from_be_bytes(buf))
    }

    /// Read an `i16` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_i16_be(&mut self) -> Result<i16, Error> {
        let mut buf = [0u8; 2];
        self.read_bytes(&mut buf)?;
        Ok(i16::from_be_bytes(buf))
    }

    /// Read the next 3 bytes as the lower 3 bytes of a `u32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_u24_be(&mut self) -> Result<u32, Error> {
        let mut buf = [0u8; 3];
        self.read_bytes(&mut buf)?;
        Ok(u32::from_be_bytes([0, buf[0], buf[1], buf[2]]))
    }

    /// Read a `u32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_u32_be(&mut self) -> Result<u32, Error> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf)?;
        Ok(u32::from_be_bytes(buf))
    }

    /// Read an `i32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_i32_be(&mut self) -> Result<i32, Error> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf)?;
        Ok(i32::from_be_bytes(buf))
    }

    /// Read a `u64` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_u64_be(&mut self) -> Result<u64, Error> {
        let mut buf = [0u8; 8];
        self.read_bytes(&mut buf)?;
        Ok(u64::from_be_bytes(buf))
    }

    /// Read an `i64` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_i64_be(&mut self) -> Result<i64, Error> {
        let mut buf = [0u8; 8];
        self.read_bytes(&mut buf)?;
        Ok(i64::from_be_bytes(buf))
    }

    /// Read a `u128` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_u128_be(&mut self) -> Result<u128, Error> {
        let mut buf = [0u8; 16];
        self.read_bytes(&mut buf)?;
        Ok(u128::from_be_bytes(buf))
    }

    /// Read an `i128` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_i128_be(&mut self) -> Result<i128, Error> {
        let mut buf = [0u8; 16];
        self.read_bytes(&mut buf)?;
        Ok(i128::from_be_bytes(buf))
    }

    /// Read an `f32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_f32_be(&mut self) -> Result<f32, Error> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf)?;
        Ok(f32::from_be_bytes(buf))
    }

    /// Read an `f64` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying reader fails.
    fn read_f64_be(&mut self) -> Result<f64, Error> {
        let mut buf = [0u8; 8];
        self.read_bytes(&mut buf)?;
        Ok(f64::from_be_bytes(buf))
    }
}

impl<T: embedded_io::Read> ReadBytesExt for T {
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.read_exact(buf).map_err(|e| match e {
            embedded_io::ReadExactError::UnexpectedEof => Error::Io(embedded_io::ErrorKind::Other),
            embedded_io::ReadExactError::Other(e) => Error::Io(e.kind()),
        })
    }
}

/// Extension trait for writing big-endian values to a byte stream.
///
/// The only required method is [`write_bytes`](WriteBytesExt::write_bytes).
/// Backed by `embedded_io::Write` via a blanket impl.
pub trait WriteBytesExt {
    /// Write all bytes from `buf` to the stream.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_bytes(&mut self, buf: &[u8]) -> Result<(), Error>;

    /// Write a single `u8`.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u8(&mut self, val: u8) -> Result<(), Error> {
        self.write_bytes(&[val])
    }

    /// Write an `i8`.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_i8(&mut self, val: i8) -> Result<(), Error> {
        self.write_bytes(&[val.cast_unsigned()])
    }

    /// Write a `u16` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u16_be(&mut self, val: u16) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write an `i16` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_i16_be(&mut self, val: i16) -> Result<(), Error> {
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

    /// Write an `i32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_i32_be(&mut self, val: i32) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write a `u64` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u64_be(&mut self, val: u64) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write an `i64` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_i64_be(&mut self, val: i64) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write a `u128` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_u128_be(&mut self, val: u128) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write an `i128` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_i128_be(&mut self, val: i128) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write an `f32` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_f32_be(&mut self, val: f32) -> Result<(), Error> {
        self.write_bytes(&val.to_be_bytes())
    }

    /// Write an `f64` in big-endian byte order.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the underlying writer fails.
    fn write_f64_be(&mut self, val: f64) -> Result<(), Error> {
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

    struct FailingReader;

    impl embedded_io::ErrorType for FailingReader {
        type Error = embedded_io::ErrorKind;
    }

    impl embedded_io::Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Self::Error> {
            Err(embedded_io::ErrorKind::BrokenPipe)
        }
    }

    // --- Error mapping ---

    #[test]
    fn write_io_error_maps_to_error_io() {
        assert!(matches!(
            FailingWriter.write_u8(0),
            Err(Error::Io(embedded_io::ErrorKind::BrokenPipe))
        ));
    }

    #[test]
    fn read_io_error_maps_to_error_io() {
        assert!(matches!(
            FailingReader.read_u8(),
            Err(Error::Io(embedded_io::ErrorKind::BrokenPipe))
        ));
    }

    // --- ReadBytesExt ---

    #[test]
    fn read_u8_decodes_correctly() {
        let buf: &[u8] = &[0xAB];
        assert_eq!((&mut &*buf).read_u8().unwrap(), 0xAB);
    }

    #[test]
    fn read_i8_decodes_correctly() {
        let buf: &[u8] = &[0xFF];
        assert_eq!((&mut &*buf).read_i8().unwrap(), -1);
    }

    #[test]
    fn read_u16_be_decodes_correctly() {
        let buf: &[u8] = &[0x01, 0x02];
        assert_eq!((&mut &*buf).read_u16_be().unwrap(), 0x0102);
    }

    #[test]
    fn read_i16_be_decodes_correctly() {
        let buf: &[u8] = &[0xFF, 0xFE];
        assert_eq!((&mut &*buf).read_i16_be().unwrap(), -2);
    }

    #[test]
    fn read_u24_be_decodes_correctly() {
        let buf: &[u8] = &[0x01, 0x02, 0x03];
        assert_eq!((&mut &*buf).read_u24_be().unwrap(), 0x0001_0203);
    }

    #[test]
    fn read_u32_be_decodes_correctly() {
        let buf: &[u8] = &[0x01, 0x02, 0x03, 0x04];
        assert_eq!((&mut &*buf).read_u32_be().unwrap(), 0x0102_0304);
    }

    #[test]
    fn read_i32_be_decodes_correctly() {
        let buf: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFE];
        assert_eq!((&mut &*buf).read_i32_be().unwrap(), -2);
    }

    #[test]
    fn read_u64_be_decodes_correctly() {
        let buf: &[u8] = &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!((&mut &*buf).read_u64_be().unwrap(), 0x0102_0304_0506_0708);
    }

    #[test]
    fn read_i64_be_decodes_correctly() {
        let buf: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE];
        assert_eq!((&mut &*buf).read_i64_be().unwrap(), -2);
    }

    #[test]
    fn read_u128_be_decodes_correctly() {
        let buf: &[u8] = &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ];
        assert_eq!((&mut &*buf).read_u128_be().unwrap(), 1);
    }

    #[test]
    fn read_i128_be_decodes_correctly() {
        let buf: &[u8] = &[
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE,
        ];
        assert_eq!((&mut &*buf).read_i128_be().unwrap(), -2);
    }

    #[test]
    fn read_f32_be_decodes_correctly() {
        let expected: f32 = 1.0;
        let buf = expected.to_be_bytes();
        assert_eq!((&mut buf.as_slice()).read_f32_be().unwrap(), expected);
    }

    #[test]
    fn read_f64_be_decodes_correctly() {
        let expected: f64 = 1.0;
        let buf = expected.to_be_bytes();
        assert_eq!((&mut buf.as_slice()).read_f64_be().unwrap(), expected);
    }

    // --- WriteBytesExt ---

    #[test]
    fn write_u8_encodes_correctly() {
        let mut buf = [0u8; 1];
        buf.as_mut_slice().write_u8(0xAB).unwrap();
        assert_eq!(buf, [0xAB]);
    }

    #[test]
    fn write_i8_encodes_correctly() {
        let mut buf = [0u8; 1];
        buf.as_mut_slice().write_i8(-1).unwrap();
        assert_eq!(buf, [0xFF]);
    }

    #[test]
    fn write_u16_be_encodes_correctly() {
        let mut buf = [0u8; 2];
        buf.as_mut_slice().write_u16_be(0x0102).unwrap();
        assert_eq!(buf, [0x01, 0x02]);
    }

    #[test]
    fn write_i16_be_encodes_correctly() {
        let mut buf = [0u8; 2];
        buf.as_mut_slice().write_i16_be(-2).unwrap();
        assert_eq!(buf, [0xFF, 0xFE]);
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

    #[test]
    fn write_i32_be_encodes_correctly() {
        let mut buf = [0u8; 4];
        buf.as_mut_slice().write_i32_be(-2).unwrap();
        assert_eq!(buf, [0xFF, 0xFF, 0xFF, 0xFE]);
    }

    #[test]
    fn write_u64_be_encodes_correctly() {
        let mut buf = [0u8; 8];
        buf.as_mut_slice()
            .write_u64_be(0x0102_0304_0506_0708)
            .unwrap();
        assert_eq!(buf, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    }

    #[test]
    fn write_i64_be_encodes_correctly() {
        let mut buf = [0u8; 8];
        buf.as_mut_slice().write_i64_be(-2).unwrap();
        assert_eq!(buf, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]);
    }

    #[test]
    fn write_u128_be_encodes_correctly() {
        let mut buf = [0u8; 16];
        buf.as_mut_slice().write_u128_be(1).unwrap();
        assert_eq!(buf, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]);
    }

    #[test]
    fn write_i128_be_encodes_correctly() {
        let mut buf = [0u8; 16];
        buf.as_mut_slice().write_i128_be(-2).unwrap();
        let expected = (-2_i128).to_be_bytes();
        assert_eq!(buf, expected);
    }

    #[test]
    fn write_f32_be_encodes_correctly() {
        let val: f32 = 1.0;
        let mut buf = [0u8; 4];
        buf.as_mut_slice().write_f32_be(val).unwrap();
        assert_eq!(buf, val.to_be_bytes());
    }

    #[test]
    fn write_f64_be_encodes_correctly() {
        let val: f64 = 1.0;
        let mut buf = [0u8; 8];
        buf.as_mut_slice().write_f64_be(val).unwrap();
        assert_eq!(buf, val.to_be_bytes());
    }

    // --- Round-trip ---

    #[test]
    fn round_trip_f32() {
        let val: f32 = core::f32::consts::PI;
        let mut buf = [0u8; 4];
        buf.as_mut_slice().write_f32_be(val).unwrap();
        assert_eq!((&mut buf.as_slice()).read_f32_be().unwrap(), val);
    }

    #[test]
    fn round_trip_f64() {
        let val: f64 = core::f64::consts::PI;
        let mut buf = [0u8; 8];
        buf.as_mut_slice().write_f64_be(val).unwrap();
        assert_eq!((&mut buf.as_slice()).read_f64_be().unwrap(), val);
    }
}
