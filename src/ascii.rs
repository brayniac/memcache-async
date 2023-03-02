//! This is a simplified implementation of [rust-memcache](https://github.com/aisk/rust-memcache)
//! ported for AsyncRead + AsyncWrite.

use futures::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::marker::Unpin;

/// Memcache ASCII protocol implementation.
pub struct Protocol<S> {
    io: BufReader<S>,
    buf: Vec<u8>,
}

impl<S> Protocol<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Creates the ASCII protocol on a stream.
    pub fn new(io: S) -> Self {
        Self {
            io: BufReader::new(io),
            buf: Vec::new(),
        }
    }

    /// Returns the value for given key as bytes. If the value doesn't exist, [`ErrorKind::NotFound`] is returned.
    pub async fn get<K: AsRef<[u8]>>(&mut self, key: K) -> Result<Vec<u8>, Error> {
        // Send command
        let writer = self.io.get_mut();
        writer
            .write_all(&[b"get ", key.as_ref(), b"\r\n"].concat())
            .await?;
        writer.flush().await?;

        // Read response header
        let header = self.read_line().await?;
        let header = std::str::from_utf8(header).map_err(|_| ErrorKind::InvalidData)?;

        // Check response header and parse value length
        if header.contains("ERROR") {
            return Err(Error::new(ErrorKind::Other, header));
        } else if header.starts_with("END") {
            return Err(ErrorKind::NotFound.into());
        }

        // VALUE <key> <flags> <bytes> [<cas unique>]\r\n
        let length: usize = header
            .split(' ')
            .nth(3)
            .and_then(|len| len.trim_end().parse().ok())
            .ok_or(ErrorKind::InvalidData)?;

        // Read value
        let mut buffer: Vec<u8> = vec![0; length];
        self.io.read_exact(&mut buffer).await?;

        // Read the trailing header
        self.read_line().await?; // \r\n
        self.read_line().await?; // END\r\n

        Ok(buffer)
    }

    /// Returns values for multiple keys in a single call as a [`HashMap`] from keys to found values.
    /// If a key is not present in memcached it will be absent from returned map.
    pub async fn get_multi<K: AsRef<[u8]>>(
        &mut self,
        keys: &[K],
    ) -> Result<HashMap<String, Vec<u8>>, Error> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }

        // Send command
        let writer = self.io.get_mut();
        writer.write_all("get".as_bytes()).await?;
        for k in keys {
            writer.write_all(b" ").await?;
            writer.write_all(k.as_ref()).await?;
        }
        writer.write_all(b"\r\n").await?;
        writer.flush().await?;

        // Read response header
        self.read_many_values().await
    }

    async fn read_many_values(&mut self) -> Result<HashMap<String, Vec<u8>>, Error> {
        let mut map = HashMap::new();
        loop {
            let header = {
                let buf = self.read_line().await?;
                std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
            }
            .to_string();
            let mut parts = header.split(' ');
            match parts.next() {
                Some("VALUE") => {
                    if let (Some(key), _flags, Some(size_str)) =
                        (parts.next(), parts.next(), parts.next())
                    {
                        let size: usize = size_str
                            .trim_end()
                            .parse()
                            .map_err(|_| Error::from(ErrorKind::InvalidData))?;
                        let mut buffer: Vec<u8> = vec![0; size];
                        self.io.read_exact(&mut buffer).await?;
                        let mut crlf = vec![0; 2];
                        self.io.read_exact(&mut crlf).await?;

                        map.insert(key.to_owned(), buffer);
                    } else {
                        return Err(Error::new(ErrorKind::InvalidData, header));
                    }
                }
                Some("END\r\n") => return Ok(map),
                Some("ERROR") => return Err(Error::new(ErrorKind::Other, header)),
                _ => return Err(Error::new(ErrorKind::InvalidData, header)),
            }
        }
    }

    /// Add a key. If the value exists, [`ErrorKind::AlreadyExists`] is returned.
    pub async fn add<K: AsRef<[u8]>>(
        &mut self,
        key: K,
        val: &[u8],
        expiration: u32,
    ) -> Result<(), Error> {
        // Send command
        let writer = self.io.get_mut();
        writer
            .write_all(
                &[
                    b"add ",
                    key.as_ref(),
                    format!(" 0 {} {}\r\n", expiration, val.len()).as_bytes(),
                    val,
                    b"\r\n",
                ]
                .concat(),
            )
            .await?;
        writer.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        // Check response header and parse value length
        if header.contains("ERROR") {
            return Err(Error::new(ErrorKind::Other, header));
        } else if header.starts_with("NOT_STORED") {
            return Err(ErrorKind::AlreadyExists.into());
        }

        Ok(())
    }

    /// Replace a key. If the value exists, [`ErrorKind::NotFound`] is returned.
    pub async fn replace<K: AsRef<[u8]>>(
        &mut self,
        key: K,
        val: &[u8],
        expiration: u32,
    ) -> Result<(), Error> {
        // Send command
        let writer = self.io.get_mut();
        writer
            .write_all(
                &[
                    b"replace ",
                    key.as_ref(),
                    format!(" 0 {} {}\r\n", expiration, val.len()).as_bytes(),
                    val,
                    b"\r\n",
                ]
                .concat(),
            )
            .await?;
        writer.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        // Check response header and parse value length
        if header.contains("ERROR") {
            return Err(Error::new(ErrorKind::Other, header));
        } else if header.starts_with("NOT_STORED") {
            return Err(ErrorKind::NotFound.into());
        }

        Ok(())
    }

    /// Set key to given value and wait for response.
    pub async fn set<K: AsRef<[u8]>>(
        &mut self,
        key: K,
        val: &[u8],
        expiration: u32,
    ) -> Result<(), Error> {
        // Send command
        let writer = self.io.get_mut();
        writer
            .write_all(
                &[
                    b"set ",
                    key.as_ref(),
                    format!(" 0 {} {}\r\n", expiration, val.len()).as_bytes(),
                    val,
                    b"\r\n",
                ]
                .concat(),
            )
            .await?;
        writer.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        // Check response header
        if header.contains("ERROR") {
            return Err(Error::new(ErrorKind::Other, header));
        }

        Ok(())
    }

    /// Increment a key by a given amount and returns the new value. If the key
    /// was not found, [`ErrorKind::NotFound`] is returned
    pub async fn incr<K: AsRef<[u8]>>(&mut self, key: K, amt: u64) -> Result<u64, Error> {
        // Send command
        let writer = self.io.get_mut();
        writer
            .write_all(&[b"incr ", key.as_ref(), format!(" {}\r\n", amt).as_bytes()].concat())
            .await?;
        writer.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        // Check response header
        if header.contains("ERROR") {
            return Err(Error::new(ErrorKind::Other, header));
        } else if header.starts_with("NOT_FOUND") {
            return Err(ErrorKind::NotFound.into());
        }

        if let Ok(v) = header.parse::<u64>() {
            Ok(v)
        } else {
            Err(Error::from(ErrorKind::InvalidData))
        }
    }

    /// Decrement a key by a given amount and returns the new value. If the key
    /// was not found, [`ErrorKind::NotFound`] is returned
    pub async fn decr<K: AsRef<[u8]>>(&mut self, key: K, amt: u64) -> Result<u64, Error> {
        // Send command
        let writer = self.io.get_mut();
        writer
            .write_all(&[b"decr ", key.as_ref(), format!(" {}\r\n", amt).as_bytes()].concat())
            .await?;
        writer.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        // Check response header
        if header.contains("ERROR") {
            return Err(Error::new(ErrorKind::Other, header));
        } else if header.starts_with("NOT_FOUND") {
            return Err(ErrorKind::NotFound.into());
        }

        if let Ok(v) = header.parse::<u64>() {
            Ok(v)
        } else {
            Err(Error::from(ErrorKind::InvalidData))
        }
    }

    /// Delete a key and wait for response. If the key was not found,
    /// [`ErrorKind::NotFound`] is returned
    pub async fn delete<K: AsRef<[u8]>>(&mut self, key: K) -> Result<(), Error> {
        // Send command
        let writer = self.io.get_mut();
        writer
            .write_all(&[b"delete ", key.as_ref(), b"\r\n"].concat())
            .await?;
        writer.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        // Check response header
        if header.contains("ERROR") {
            return Err(Error::new(ErrorKind::Other, header));
        } else if header.starts_with("NOT_FOUND") {
            return Err(ErrorKind::NotFound.into());
        }

        Ok(())
    }

    /// Return the version of the remote server.
    pub async fn version(&mut self) -> Result<String, Error> {
        self.io.write_all(b"version\r\n").await?;
        self.io.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        if !header.starts_with("VERSION") {
            return Err(Error::new(ErrorKind::Other, header));
        }
        let version = header.trim_start_matches("VERSION ").trim_end();
        Ok(version.to_string())
    }

    /// Delete all keys from the cache.
    pub async fn flush(&mut self) -> Result<(), Error> {
        self.io.write_all(b"flush_all\r\n").await?;
        self.io.flush().await?;

        // Read response header
        let header = {
            let buf = self.read_line().await?;
            std::str::from_utf8(buf).map_err(|_| Error::from(ErrorKind::InvalidData))?
        };

        if header == "OK\r\n" {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::Other, header))
        }
    }

    async fn read_line(&mut self) -> Result<&[u8], Error> {
        let Self { io, buf } = self;
        buf.clear();
        io.read_until(b'\n', buf).await?;
        if buf.last().copied() != Some(b'\n') {
            return Err(ErrorKind::UnexpectedEof.into());
        }
        Ok(&buf[..])
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use futures::io::{AsyncRead, AsyncWrite};
    use std::io::{Cursor, Error, ErrorKind, Read, Write};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct Cache {
        r: Cursor<Vec<u8>>,
        w: Cursor<Vec<u8>>,
    }

    impl Cache {
        fn new() -> Self {
            Cache {
                r: Cursor::new(Vec::new()),
                w: Cursor::new(Vec::new()),
            }
        }
    }

    impl AsyncRead for Cache {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context,
            buf: &mut [u8],
        ) -> Poll<Result<usize, Error>> {
            Poll::Ready(self.get_mut().r.read(buf))
        }
    }

    impl AsyncWrite for Cache {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context,
            buf: &[u8],
        ) -> Poll<Result<usize, Error>> {
            Poll::Ready(self.get_mut().w.write(buf))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Error>> {
            Poll::Ready(self.get_mut().w.flush())
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn test_ascii_get() {
        let mut cache = Cache::new();
        cache
            .r
            .get_mut()
            .extend_from_slice(b"VALUE foo 0 3\r\nbar\r\nEND\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        assert_eq!(block_on(ascii.get(&"foo")).unwrap(), b"bar");
        assert_eq!(cache.w.get_ref(), b"get foo\r\n");
    }

    #[test]
    fn test_ascii_get2() {
        let mut cache = Cache::new();
        cache
            .r
            .get_mut()
            .extend_from_slice(b"VALUE foo 0 3\r\nbar\r\nEND\r\nVALUE bar 0 3\r\nbaz\r\nEND\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        assert_eq!(block_on(ascii.get(&"foo")).unwrap(), b"bar");
        assert_eq!(block_on(ascii.get(&"bar")).unwrap(), b"baz");
    }

    #[test]
    fn test_ascii_get_cas() {
        let mut cache = Cache::new();
        cache
            .r
            .get_mut()
            .extend_from_slice(b"VALUE foo 0 3 99999\r\nbar\r\nEND\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        assert_eq!(block_on(ascii.get(&"foo")).unwrap(), b"bar");
        assert_eq!(cache.w.get_ref(), b"get foo\r\n");
    }

    #[test]
    fn test_ascii_get_empty() {
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"END\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        assert_eq!(
            block_on(ascii.get(&"foo")).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert_eq!(cache.w.get_ref(), b"get foo\r\n");
    }

    #[test]
    fn test_ascii_get_eof_error() {
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"EN");
        let mut ascii = super::Protocol::new(&mut cache);
        assert_eq!(
            block_on(ascii.get(&"foo")).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
        assert_eq!(cache.w.get_ref(), b"get foo\r\n");
    }

    #[test]
    fn test_ascii_get_one() {
        let mut cache = Cache::new();
        cache
            .r
            .get_mut()
            .extend_from_slice(b"VALUE foo 0 3\r\nbar\r\nEND\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        let keys = vec!["foo"];
        let map = block_on(ascii.get_multi(&keys)).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("foo").unwrap(), b"bar");
        assert_eq!(cache.w.get_ref(), b"get foo\r\n");
    }

    #[test]
    fn test_ascii_get_many() {
        let mut cache = Cache::new();
        cache
            .r
            .get_mut()
            .extend_from_slice(b"VALUE foo 0 3\r\nbar\r\nVALUE baz 44 4\r\ncrux\r\nEND\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        let keys = vec!["foo", "baz", "blah"];
        let map = block_on(ascii.get_multi(&keys)).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("foo").unwrap(), b"bar");
        assert_eq!(map.get("baz").unwrap(), b"crux");
        assert_eq!(cache.w.get_ref(), b"get foo baz blah\r\n");
    }

    #[test]
    fn test_ascii_get_multi_empty() {
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"END\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        let keys = vec!["foo", "baz"];
        let map = block_on(ascii.get_multi(&keys)).unwrap();
        assert!(map.is_empty());
        assert_eq!(cache.w.get_ref(), b"get foo baz\r\n");
    }

    #[test]
    fn test_ascii_get_multi_zero_keys() {
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"END\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        let map = block_on(ascii.get_multi::<&str>(&[])).unwrap();
        assert!(map.is_empty());
        assert_eq!(cache.w.get_ref(), b"");
    }

    #[test]
    fn test_ascii_set() {
        let (key, val, ttl) = ("foo", "bar", 5);
        let mut cache = Cache::new();
        let mut ascii = super::Protocol::new(&mut cache);
        block_on(ascii.set(&key, val.as_bytes(), ttl)).unwrap();
        assert_eq!(
            cache.w.get_ref(),
            &format!("set {} 0 {} {} noreply\r\n{}\r\n", key, ttl, val.len(), val)
                .as_bytes()
                .to_vec()
        );
    }

    #[test]
    fn test_ascii_add_new_key() {
        let (key, val, ttl) = ("foo", "bar", 5);
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"STORED\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        block_on(ascii.add(&key, val.as_bytes(), ttl)).unwrap();
        assert_eq!(
            cache.w.get_ref(),
            &format!("add {} 0 {} {}\r\n{}\r\n", key, ttl, val.len(), val)
                .as_bytes()
                .to_vec()
        );
    }

    #[test]
    fn test_ascii_add_duplicate() {
        let (key, val, ttl) = ("foo", "bar", 5);
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"NOT_STORED\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        assert_eq!(
            block_on(ascii.add(&key, val.as_bytes(), ttl))
                .unwrap_err()
                .kind(),
            ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn test_ascii_version() {
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"VERSION 1.6.6\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        assert_eq!(block_on(ascii.version()).unwrap(), "1.6.6");
        assert_eq!(cache.w.get_ref(), b"version\r\n");
    }

    #[test]
    fn test_ascii_flush() {
        let mut cache = Cache::new();
        cache.r.get_mut().extend_from_slice(b"OK\r\n");
        let mut ascii = super::Protocol::new(&mut cache);
        assert!(block_on(ascii.flush()).is_ok());
        assert_eq!(cache.w.get_ref(), b"flush_all\r\n");
    }
}
