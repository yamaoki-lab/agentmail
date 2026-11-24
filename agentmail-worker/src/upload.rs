//! Bounded, length-prefixed stdin frames for a patch and optional mail body.
use std::io::{self, Read, Write};

pub const MAX_BODY: u64 = 256 * 1024;
pub const MAX_PATCH: u64 = 6 * 1024 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub fn read_limited(reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("input exceeds size limit"));
    }
    Ok(bytes)
}

pub fn read_frame(reader: &mut impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut length = [0; 8];
    reader.read_exact(&mut length)?;
    let length = u64::from_be_bytes(length);
    if length > limit {
        return Err(invalid("upload frame exceeds size limit"));
    }
    let mut data = vec![0; length as usize];
    reader.read_exact(&mut data)?;
    Ok(data)
}

pub fn write_frame(writer: &mut impl Write, data: &[u8]) -> io::Result<()> {
    writer.write_all(&(data.len() as u64).to_be_bytes())?;
    writer.write_all(data)
}

pub fn expect_end(reader: &mut impl Read) -> io::Result<()> {
    if reader.read(&mut [0])? != 0 {
        return Err(invalid("unexpected data after upload frames"));
    }
    Ok(())
}

pub fn valid_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversize_truncated_and_trailing_frames() {
        let mut frame = Vec::new();
        write_frame(&mut frame, b"abc").unwrap();
        assert!(read_frame(&mut frame.as_slice(), 2).is_err());
        assert!(read_frame(&mut &frame[..10], MAX_PATCH).is_err());
        assert_eq!(read_frame(&mut frame.as_slice(), 3).unwrap(), b"abc");
        let mut data = frame.as_slice();
        assert!(expect_end(&mut data).is_err());
    }
}
