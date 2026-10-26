use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{ListenerOptions, Name};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::debug;

use crate::{Frame, IPC_VERSION, MAX_IPC_FRAME, Request, Response};

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("malformed message: {0}")]
    Codec(String),
    #[error("ipc version {theirs} is not supported (expected {IPC_VERSION}); update Bridge")]
    Version { theirs: u16 },
    #[error("message of {0} bytes is too large")]
    TooLarge(usize),
}

/// Platform default endpoint for the current user (ARCHITECTURE §2.3).
pub fn default_endpoint() -> String {
    if cfg!(windows) {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        format!("bridge-{}", sanitize(&user))
    } else {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("bridge")
            .join("ipc.sock")
            .to_string_lossy()
            .into_owned()
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn name(endpoint: &str) -> io::Result<Name<'_>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        endpoint.to_ns_name::<GenericNamespaced>()
    }
    #[cfg(not(windows))]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        endpoint.to_fs_name::<GenericFilePath>()
    }
}

async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    id: u64,
    body: T,
) -> Result<(), IpcError> {
    let bytes = postcard::to_allocvec(&Frame {
        version: IPC_VERSION,
        id,
        body,
    })
    .map_err(|e| IpcError::Codec(e.to_string()))?;
    let len = u32::try_from(bytes.len()).map_err(|_| IpcError::TooLarge(bytes.len()))?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// `Ok(None)` on a clean end of stream.
async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> Result<Option<Frame<T>>, IpcError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_IPC_FRAME {
        return Err(IpcError::TooLarge(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    // Peek at the version before decoding the body, so old clients get a clear error.
    let version: u16 = postcard::take_from_bytes(&buf)
        .map(|(v, _)| v)
        .map_err(|e| IpcError::Codec(e.to_string()))?;
    if version != IPC_VERSION {
        return Err(IpcError::Version { theirs: version });
    }
    postcard::from_bytes(&buf)
        .map(Some)
        .map_err(|e| IpcError::Codec(e.to_string()))
}

pub type IpcHandler =
    Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync>;

pub struct IpcServer {
    listener: Listener,
}

impl IpcServer {
    pub fn bind(endpoint: &str) -> Result<Self, IpcError> {
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(dir) = std::path::Path::new(endpoint).parent() {
                std::fs::create_dir_all(dir)?;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let options = ListenerOptions::new()
            .name(name(endpoint)?)
            .reclaim_name(true)
            .try_overwrite(true);
        #[cfg(windows)]
        let options = {
            use interprocess::os::windows::local_socket::ListenerOptionsExt;
            use interprocess::os::windows::security_descriptor::SecurityDescriptor;
            // Protected DACL: full access for SYSTEM and the pipe's owner only.
            let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;OW)")
                .map_err(|e| IpcError::Codec(e.to_string()))?;
            options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
        };
        Ok(Self {
            listener: options.create_tokio()?,
        })
    }

    /// Serves clients until the task is dropped; each client gets its own task.
    pub async fn serve(self, handler: IpcHandler) {
        loop {
            match self.listener.accept().await {
                Ok(stream) => {
                    let handler = handler.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_client(stream, handler).await {
                            debug!("ipc client: {e}");
                        }
                    });
                }
                Err(e) => {
                    debug!("ipc accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }
}

async fn serve_client(mut stream: Stream, handler: IpcHandler) -> Result<(), IpcError> {
    loop {
        let frame = match read_frame::<_, Request>(&mut stream).await {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(()),
            Err(IpcError::Version { theirs }) => {
                let msg =
                    format!("ipc version {theirs} is not supported; daemon speaks {IPC_VERSION}");
                write_frame(&mut stream, 0, Response::Error(msg)).await?;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let response = handler(frame.body).await;
        write_frame(&mut stream, frame.id, response).await?;
    }
}

pub struct IpcClient {
    stream: Stream,
    next_id: u64,
}

impl IpcClient {
    pub async fn connect(endpoint: &str) -> Result<Self, IpcError> {
        Ok(Self {
            stream: Stream::connect(name(endpoint)?).await?,
            next_id: 1,
        })
    }

    pub async fn request(&mut self, request: Request) -> Result<Response, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.stream, id, request).await?;
        match read_frame::<_, Response>(&mut self.stream).await? {
            Some(f) if f.id == id || f.id == 0 => Ok(f.body),
            Some(_) => Err(IpcError::Codec("response id mismatch".into())),
            None => Err(IpcError::Io(io::ErrorKind::UnexpectedEof.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip_and_reject_oversize_and_foreign_version() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        write_frame(&mut a, 7, Request::Status).await.unwrap();
        let f: Frame<Request> = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!((f.id, f.body), (7, Request::Status));

        a.write_all(&(MAX_IPC_FRAME as u32 + 1).to_le_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, Request>(&mut b).await,
            Err(IpcError::TooLarge(_))
        ));

        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let bytes = postcard::to_allocvec(&Frame {
            version: 99u16,
            id: 1,
            body: Request::Status,
        })
        .unwrap();
        a.write_all(&(bytes.len() as u32).to_le_bytes())
            .await
            .unwrap();
        a.write_all(&bytes).await.unwrap();
        assert!(matches!(
            read_frame::<_, Request>(&mut b).await,
            Err(IpcError::Version { theirs: 99 })
        ));

        drop(a);
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        assert!(read_frame::<_, Request>(&mut b).await.unwrap().is_none());
    }

    #[test]
    fn endpoint_names_are_sanitised() {
        assert_eq!(sanitize("Ив ан\\x"), "______x");
        assert_eq!(sanitize("user-pc_1"), "user-pc_1");
        assert!(!default_endpoint().is_empty());
    }
}
