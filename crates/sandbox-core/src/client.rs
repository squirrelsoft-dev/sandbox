use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::api::{Request, Response};

/// Send one request to sandboxd and decode the response payload.
pub async fn call<T: DeserializeOwned>(socket: &Path, req: &Request) -> Result<T> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "cannot reach sandboxd at {} (is it running? try `sandboxd` or `launchctl kickstart gui/$(id -u)/dev.squirrelsoft.sandboxd`)",
            socket.display()
        )
    })?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.shutdown().await?;

    let mut reply = String::new();
    BufReader::new(read).read_line(&mut reply).await?;
    if reply.is_empty() {
        bail!("sandboxd closed the connection without replying");
    }
    let resp: Response = serde_json::from_str(&reply).context("malformed reply from sandboxd")?;
    if !resp.ok {
        bail!("{}", resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(serde_json::from_value(resp.data.unwrap_or(serde_json::Value::Null))?)
}
