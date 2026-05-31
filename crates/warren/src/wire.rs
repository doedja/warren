use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_FRAME: usize = 1 << 20;

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let bytes = warren_proto::encode(msg);
    if bytes.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let len = (bytes.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_msg<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut header = [0u8; 4];
    r.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    warren_proto::decode(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_hello() {
        let (mut client, mut server) = duplex(1024);

        let hello = warren_proto::Hello {
            protocol_version: warren_proto::PROTOCOL_VERSION,
            token: "tok".into(),
            node_name: "n".into(),
            platform: warren_proto::Platform::Linux,
            agent_version: "0.1.0".into(),
        };
        write_msg(&mut client, &hello).await.unwrap();

        let received: warren_proto::Hello = read_msg(&mut server).await.unwrap();
        assert_eq!(hello, received);
    }
}
