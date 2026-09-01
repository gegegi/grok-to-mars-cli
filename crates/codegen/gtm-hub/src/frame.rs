use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        bail!("hub frame too large: {len} (max {MAX_FRAME})");
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> Result<()> {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    if len > MAX_FRAME {
        bail!("hub frame too large: {len} (max {MAX_FRAME})");
    }
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(data).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip() {
        let (mut a, mut b) = duplex(64);
        write_frame(&mut a, br#"{"jsonrpc":"2.0"}"#).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, br#"{"jsonrpc":"2.0"}"#);
    }

    #[tokio::test]
    async fn rejects_oversize() {
        let (mut a, mut b) = duplex(16);
        let huge = MAX_FRAME + 1;
        a.write_all(&huge.to_be_bytes()).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
    }
}
