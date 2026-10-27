//! Unix socket front end. One JSON request per connection.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use sandbox_core::api::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::manager::Manager;

pub async fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            bail!("another sandboxd is already listening on {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve(listener: UnixListener, mgr: Arc<Manager>) {
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == me => {}
            Ok(cred) => {
                tracing::warn!(uid = cred.uid(), "rejecting client from another user");
                continue;
            }
            Err(e) => {
                tracing::debug!(error = %e, "cannot read peer credentials");
                continue;
            }
        }
        let mgr = mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &mgr).await {
                tracing::warn!(error = %format!("{e:#}"), "client error");
            }
        });
    }
}

async fn handle(stream: UnixStream, mgr: &Manager) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => dispatch(req, mgr).await.unwrap_or_else(|e| Response::err(format!("{e:#}"))),
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    let mut out = serde_json::to_string(&resp)?;
    out.push('\n');
    write.write_all(out.as_bytes()).await?;
    Ok(())
}

async fn dispatch(req: Request, mgr: &Manager) -> Result<Response> {
    Ok(match req {
        Request::Ping => Response::ok(&env!("CARGO_PKG_VERSION")),
        Request::Profiles => Response::ok(mgr.profiles()),
        Request::Agents => Response::ok(mgr.presets()),
        Request::Create(c) => Response::ok(&mgr.create(c).await?),
        Request::List => Response::ok(&mgr.list()?),
        Request::Get { selector } => Response::ok(&mgr.resolve(&selector)?),
        Request::Destroy { selector, discard } => Response::ok(&mgr.destroy(&selector, discard).await?),
        Request::Policy { selector, add, remove } => {
            Response::ok(&mgr.update_policy(&selector, &add, &remove)?)
        }
    })
}
