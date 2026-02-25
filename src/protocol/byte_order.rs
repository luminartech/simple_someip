use crate::protocol::Error;

/// Extension trait for reading big-endian integers from a byte stream.
///
/// This trait is intentionally decoupled from `std::io::Read` so it can
/// later be backed by `embedded_io::Read` without changing call sites.
/// The only required method is [`read_bytes`](ReadBytesExt::read_bytes),
/// which is adapted to `std::io::Read` via a blanket impl.
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

impl<T: std::io::Read> ReadBytesExt for T {
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.read_exact(buf)?;
        Ok(())
    }
}

/// Extension trait for writing big-endian integers to a byte stream.
///
/// This trait is intentionally decoupled from `std::io::Write` so it can
/// later be backed by `embedded_io::Write` without changing call sites.
/// The only required method is [`write_bytes`](WriteBytesExt::write_bytes),
/// which is adapted to `std::io::Write` via a blanket impl.
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

impl<T: std::io::Write> WriteBytesExt for T {
    fn write_bytes(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.write_all(buf)?;
        Ok(())
    }
}
