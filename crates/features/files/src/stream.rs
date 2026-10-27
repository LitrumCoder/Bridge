//! Bulk stream I/O: one stream per item, a short header, then raw bytes. Both sides
//! hash the whole file with BLAKE3 (a resumed prefix is re-read locally), so a resume
//! that went wrong is caught before the file is put in place.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bridge_protocol::files::BulkHeader;
use bridge_protocol::{decode_payload, encode_payload};
use bridge_transport::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::fsops::{mtime_ms, part_path};

/// Read and write in blocks of this size: files of any size stream through memory.
pub(crate) const BLOCK: usize = 256 * 1024;
const MAX_HEADER: usize = 4 * 1024;

pub(crate) async fn write_header(s: &mut dyn SendStream, h: &BulkHeader) -> Result<(), String> {
    let body = encode_payload(h).map_err(|e| e.to_string())?;
    let mut buf = (body.len() as u16).to_be_bytes().to_vec();
    buf.extend_from_slice(&body);
    s.write_all(&buf).await.map_err(|e| e.to_string())
}

async fn read_exact(s: &mut dyn RecvStream, buf: &mut [u8]) -> Result<(), String> {
    let mut got = 0;
    while got < buf.len() {
        match s.read(&mut buf[got..]).await.map_err(|e| e.to_string())? {
            Some(n) => got += n,
            None => return Err("stream ended early".into()),
        }
    }
    Ok(())
}

pub(crate) async fn read_header(s: &mut dyn RecvStream) -> Result<BulkHeader, String> {
    let mut len = [0u8; 2];
    read_exact(s, &mut len).await?;
    let len = usize::from(u16::from_be_bytes(len));
    if len > MAX_HEADER {
        return Err("bulk header too long".into());
    }
    let mut body = vec![0u8; len];
    read_exact(s, &mut body).await?;
    decode_payload(&body).map_err(|e| e.to_string())
}

/// Hashes the first `len` bytes of `file` from the start.
async fn hash_prefix(
    file: &mut tokio::fs::File,
    len: u64,
    hasher: &mut blake3::Hasher,
) -> Result<(), String> {
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(|e| e.to_string())?;
    let mut left = len;
    let mut buf = vec![0u8; BLOCK];
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = file
            .read(&mut buf[..want])
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("file shorter than expected".into());
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(())
}

pub(crate) struct Sent {
    pub size: u64,
    pub mtime_ms: i64,
    pub hash: [u8; 32],
}

pub(crate) enum SendOutcome {
    Done(Sent),
    /// The source no longer matches the offer: `(size, mtime)`, or gone.
    Changed(Option<(u64, i64)>),
}

/// Streams `source` from `offset` after checking it still is what was offered.
pub(crate) async fn send_file(
    stream: &mut dyn SendStream,
    header: BulkHeader,
    source: &Path,
    expect: (u64, i64),
    progress: &AtomicU64,
) -> Result<SendOutcome, String> {
    let meta = match tokio::fs::metadata(source).await {
        Ok(m) => m,
        Err(_) => return Ok(SendOutcome::Changed(None)),
    };
    let now = (meta.len(), mtime_ms(&meta));
    if now != expect || header.offset > now.0 {
        return Ok(SendOutcome::Changed(Some(now)));
    }
    let mut file = tokio::fs::File::open(source)
        .await
        .map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    hash_prefix(&mut file, header.offset, &mut hasher).await?;
    write_header(stream, &header).await?;
    let mut buf = vec![0u8; BLOCK];
    let mut sent = header.offset;
    while sent < now.0 {
        let want = buf.len().min((now.0 - sent) as usize);
        let n = file
            .read(&mut buf[..want])
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(SendOutcome::Changed(Some((sent, now.1))));
        }
        hasher.update(&buf[..n]);
        stream
            .write_all(&buf[..n])
            .await
            .map_err(|e| e.to_string())?;
        sent += n as u64;
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    stream.finish().map_err(|e| e.to_string())?;
    Ok(SendOutcome::Done(Sent {
        size: now.0,
        mtime_ms: now.1,
        hash: *hasher.finalize().as_bytes(),
    }))
}

/// Where one expected stream goes.
#[derive(Clone, Debug)]
pub(crate) struct Expect {
    pub dest: PathBuf,
    pub offset: u64,
    pub size: u64,
    pub progress: Arc<AtomicU64>,
}

/// Writes the stream's bytes into the partial file of `expect.dest`, starting at
/// `expect.offset`; returns the hash of the whole partial file.
pub(crate) async fn receive_file(
    stream: &mut dyn RecvStream,
    expect: &Expect,
) -> Result<[u8; 32], String> {
    let part = part_path(&expect.dest);
    let mut file = tokio::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&part)
        .await
        .map_err(|e| e.to_string())?;
    file.set_len(expect.offset)
        .await
        .map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    hash_prefix(&mut file, expect.offset, &mut hasher).await?;
    file.seek(std::io::SeekFrom::Start(expect.offset))
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; BLOCK];
    let mut have = expect.offset;
    loop {
        let Some(n) = stream.read(&mut buf).await.map_err(|e| e.to_string())? else {
            break;
        };
        if have + n as u64 > expect.size {
            return Err("the peer sent more than the file size".into());
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        have += n as u64;
        expect.progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    file.flush().await.map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    if have != expect.size {
        return Err(format!("stream ended at {have} of {} bytes", expect.size));
    }
    Ok(*hasher.finalize().as_bytes())
}
