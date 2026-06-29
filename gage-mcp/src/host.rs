//! Per-process HTTP MCP host. One `127.0.0.1:<ephemeral>` TCP listener
//! routes incoming requests by UUID path prefix to a registered
//! [`tower::Service`] (a streamable-HTTP MCP service). Each
//! `call_agent` invocation registers a per-call service; the returned
//! [`ServiceHandle`] gives the URL to hand to `claude --mcp-config`
//! and removes the registration on drop.
//!
//! Why an HTTP transport (and not stdio): stdio MCP requires the MCP
//! server to be a child of `claude`, but our per-call services live in
//! the parent `gage scan` / `gage agent` process so they have direct
//! access to the Rune VM (for scanner-defined tool callbacks). The
//! UUID path segment is the per-call capability; localhost-only bind
//! closes off off-host access.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};

use bytes::Bytes;
use http_body_util::Full;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::ServiceExt;
use tower::util::BoxCloneSyncService;
use uuid::Uuid;

/// Service shape every per-call MCP service is erased to in the
/// registry. Matches the tower::Service rmcp's
/// `StreamableHttpService` impls.
pub type RegisteredService =
    BoxCloneSyncService<Request<Incoming>, Response<BoxBody<Bytes, Infallible>>, Infallible>;

type Registry = Arc<Mutex<HashMap<Uuid, RegisteredService>>>;

/// One running HTTP MCP host. Owns the TCP listener task and the
/// per-call service registry.
pub struct McpHost {
    addr: SocketAddr,
    registry: Registry,
    shutdown: Option<oneshot::Sender<()>>,
}

#[derive(Debug)]
pub enum HostError {
    Bind(std::io::Error),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Bind(e) => write!(f, "bind 127.0.0.1: {e}"),
        }
    }
}

impl std::error::Error for HostError {}

impl McpHost {
    /// Bind on `127.0.0.1:0` and spawn the server task. Returns once
    /// the listener is accepting connections.
    pub async fn start() -> Result<Self, HostError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(HostError::Bind)?;
        let addr = listener.local_addr().map_err(HostError::Bind)?;
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let dispatch_registry = Arc::clone(&registry);
        tokio::spawn(async move {
            run_accept_loop(listener, shutdown_rx, move |req: Request<Incoming>| {
                dispatch(req, Arc::clone(&dispatch_registry))
            })
            .await;
        });
        Ok(Self {
            addr,
            registry,
            shutdown: Some(shutdown_tx),
        })
    }

    /// Listener address. Per-call URLs include this and a UUID path
    /// segment — see [`ServiceHandle::url`].
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Register a per-call MCP service. Returns a handle whose `url()`
    /// is what `claude --mcp-config` should point at. Dropping the
    /// handle unregisters the service.
    pub fn register(&self, service: RegisteredService) -> ServiceHandle {
        let uuid = Uuid::new_v4();
        self.registry.lock().unwrap().insert(uuid, service);
        let url = format!("http://{}/{}/mcp", self.addr, uuid);
        ServiceHandle {
            uuid,
            url,
            registry: Arc::downgrade(&self.registry),
        }
    }
}

impl Drop for McpHost {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            // SendError carries our `()` payload back when the server
            // task already exited — there's nothing left to notify.
            #[allow(clippy::unused_result_ok)]
            tx.send(()).ok();
        }
    }
}

/// Per-call registration receipt. Holds the URL to hand to `claude`
/// and a weak ref back to the registry so `Drop` removes the entry.
pub struct ServiceHandle {
    uuid: Uuid,
    url: String,
    registry: Weak<Mutex<HashMap<Uuid, RegisteredService>>>,
}

impl ServiceHandle {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut map) = registry.lock()
        {
            map.remove(&self.uuid);
        }
    }
}

/// Run a hyper accept loop until `shutdown` resolves. Each accepted
/// connection is served by `dispatch_fn`, which constructs a per-
/// request response. Shared by [`McpHost::start`] (UUID-routed
/// dispatch) and [`serve_http`] (fixed-path dispatch) so both paths
/// go through the same accept + serve_connection plumbing.
async fn run_accept_loop<F, Fut>(
    listener: TcpListener,
    mut shutdown: oneshot::Receiver<()>,
    dispatch_fn: F,
) where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Response<BoxBody<Bytes, Infallible>>, Infallible>>
        + Send
        + 'static,
{
    loop {
        let accept = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accept = listener.accept() => accept,
        };
        let (stream, _peer) = match accept {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("mcp host accept: {e}");
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let dispatch_fn = dispatch_fn.clone();
        let svc = service_fn(move |req: Request<Incoming>| dispatch_fn(req));
        tokio::spawn(async move {
            if let Err(e) = AutoBuilder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                tracing::debug!("mcp host serve_connection: {e}");
            }
        });
    }
}

/// Run a long-lived HTTP MCP server bound to `addr`, exposing one
/// [`GageServer`] at the fixed path `/mcp`. Returns when `shutdown`
/// resolves (the caller wires SIGINT or whatever stop signal it has).
///
/// Shares the accept loop + hyper plumbing with [`McpHost`] — only
/// the per-connection dispatch differs (here: every request is
/// forwarded to one streamable-HTTP service, no UUID routing).
pub async fn serve_http(
    addr: SocketAddr,
    shutdown: oneshot::Receiver<()>,
) -> Result<SocketAddr, HostError> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };

    let listener = TcpListener::bind(addr).await.map_err(HostError::Bind)?;
    let bound = listener.local_addr().map_err(HostError::Bind)?;

    let svc = Arc::new(StreamableHttpService::new(
        || Ok(crate::GageServer::new()),
        LocalSessionManager::default().into(),
        Default::default(),
    ));

    let serve_svc = Arc::clone(&svc);
    let dispatch_fn = move |req: Request<Incoming>| {
        let svc = Arc::clone(&serve_svc);
        async move { Ok(svc.handle(req).await) }
    };

    tokio::spawn(async move {
        run_accept_loop(listener, shutdown, dispatch_fn).await;
    });

    Ok(bound)
}

/// Top-level request handler: parse the first path segment as a UUID,
/// look up the registered service, strip the `/<uuid>` prefix, and
/// delegate. Unknown UUIDs return 404 so the surface doesn't leak
/// "this UUID existed once" through differential responses.
async fn dispatch(
    req: Request<Incoming>,
    registry: Registry,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let path = req.uri().path();
    let Some((uuid_str, rest)) = strip_uuid_prefix(path) else {
        return Ok(not_found());
    };
    let Ok(uuid) = uuid_str.parse::<Uuid>() else {
        return Ok(not_found());
    };
    let svc = {
        let map = registry.lock().unwrap();
        map.get(&uuid).cloned()
    };
    let Some(svc) = svc else {
        return Ok(not_found());
    };
    let req = rebuild_request(req, &rest);
    svc.oneshot(req).await
}

/// Split `/<uuid>/<rest>` into `(uuid_str, /<rest>)`. Returns `None`
/// when the path has no leading UUID segment.
fn strip_uuid_prefix(path: &str) -> Option<(&str, String)> {
    let trimmed = path.strip_prefix('/')?;
    match trimmed.split_once('/') {
        Some((first, rest)) => Some((first, format!("/{rest}"))),
        None => Some((trimmed, "/".to_string())),
    }
}

fn rebuild_request(req: Request<Incoming>, new_path: &str) -> Request<Incoming> {
    let (mut parts, body) = req.into_parts();
    let new_uri = match parts.uri.path_and_query() {
        Some(pq) => match pq.query() {
            Some(q) => format!("{new_path}?{q}"),
            None => new_path.to_string(),
        },
        None => new_path.to_string(),
    };
    if let Ok(uri) = new_uri.parse() {
        parts.uri = uri;
    }
    Request::from_parts(parts, body)
}

fn not_found() -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(BoxBody::new(Full::new(Bytes::new())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_uuid_prefix_trivial() {
        assert_eq!(
            strip_uuid_prefix("/abc/mcp"),
            Some(("abc", "/mcp".to_string()))
        );
        assert_eq!(strip_uuid_prefix("/abc"), Some(("abc", "/".to_string())));
        assert_eq!(
            strip_uuid_prefix("/abc/mcp/extra"),
            Some(("abc", "/mcp/extra".to_string()))
        );
        assert_eq!(strip_uuid_prefix("abc/mcp"), None);
        assert_eq!(strip_uuid_prefix(""), None);
    }

    #[tokio::test]
    async fn unknown_uuid_returns_404() {
        let host = McpHost::start().await.unwrap();
        let port = host.addr().port();
        let url = format!("http://127.0.0.1:{port}/nope/mcp");
        let resp = reqwest_get(&url).await;
        assert_eq!(resp.status, 404);
    }

    /// Minimal hyper client just for tests — avoids pulling reqwest
    /// into the crate's runtime deps just for this assertion.
    struct Resp {
        status: u16,
    }
    async fn reqwest_get(url: &str) -> Resp {
        use hyper_util::client::legacy::Client;
        use hyper_util::client::legacy::connect::HttpConnector;
        let client: Client<HttpConnector, Full<Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let req = Request::builder()
            .uri(url)
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = client.request(req).await.unwrap();
        Resp {
            status: resp.status().as_u16(),
        }
    }
}
