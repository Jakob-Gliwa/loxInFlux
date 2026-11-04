//! Execd writes must not park a Tokio blocking worker on a full pipe: otherwise
//! even a bounded application shutdown can hang during runtime destruction.
use std::io;
use tokio::io::AsyncWriteExt;

#[cfg(unix)]
pub enum Output {
    Pipe(tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>),
    File(tokio::fs::File),
}
#[cfg(unix)]
impl Output {
    pub fn open() -> io::Result<Self> {
        let fd = rustix::io::dup(std::io::stdout())?;
        let stat = rustix::fs::fstat(&fd)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::RegularFile {
            return Ok(Self::File(tokio::fs::File::from_std(std::fs::File::from(
                fd,
            ))));
        }
        Self::pipe(fd)
    }
    fn pipe(fd: std::os::fd::OwnedFd) -> io::Result<Self> {
        let flags = rustix::fs::fcntl_getfl(&fd)?;
        rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
        // SAFETY: OwnedFd exclusively owns a valid duplicated descriptor; no
        // code replaces or closes it before AsyncFd deregisters and drops it.
        let registered = unsafe {
            tokio::io::unix::AsyncFd::register_with_interest(fd, tokio::io::Interest::WRITABLE)
        }?;
        Ok(Self::Pipe(registered))
    }
    pub async fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::File(file) => file.write_all(bytes).await,
            Self::Pipe(pipe) => {
                while !bytes.is_empty() {
                    let mut guard = pipe.writable().await?;
                    match guard.try_io(|inner| {
                        rustix::io::write(inner.get_ref(), bytes).map_err(io::Error::from)
                    }) {
                        Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                        Ok(Ok(n)) => bytes = &bytes[n..],
                        Ok(Err(e)) => return Err(e),
                        Err(_) => {}
                    }
                }
                Ok(())
            }
        }
    }
    pub async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(f) => f.flush().await,
            Self::Pipe(_) => Ok(()),
        }
    }
}

#[cfg(not(unix))]
pub struct Output(tokio::io::Stdout);
#[cfg(not(unix))]
impl Output {
    pub fn open() -> io::Result<Self> {
        Ok(Self(tokio::io::stdout()))
    }
    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes).await
    }
    pub async fn flush(&mut self) -> io::Result<()> {
        self.0.flush().await
    }
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn full_execd_pipe_is_cancellable() {
        // A Unix stream has the same writable-readiness contract as a pipe.
        let (writer, _reader) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut out = Output::pipe(writer.into()).unwrap();
        let data = vec![b'x'; 8 * 1024 * 1024];
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), out.write_all(&data))
                .await
                .is_err()
        );
    }
}
