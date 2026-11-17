//! The connections of a daemon.
//!
//! Each connection is served by hyper, with what a server that anyone
//! can reach has to have and a server out of the box does not: a limit
//! on how long the head of a request may take to arrive, so that a
//! connection that says nothing does not stay.

use core::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::daemon::raised;

/// How long the head of a request may take to arrive.
const HEAD_WITHIN: Duration = Duration::from_secs(10);

/// How long a daemon that cannot take a connection waits before it
/// tries again: it has too many files open, and one may close.
const TRY_AGAIN: Duration = Duration::from_millis(50);

/// Serves one connection until it closes, or until the daemon stops and
/// what is under way on it has been answered.
async fn connection(stream: TcpStream, router: Router, mut stopped: watch::Receiver<bool>) {
    // What a session tells is small and is to arrive when it is told.
    let _ = stream.set_nodelay(true);
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(HEAD_WITHIN);
    let serving = http.serve_connection(TokioIo::new(stream), TowerToHyperService::new(router));
    tokio::pin!(serving);
    tokio::select! {
        // A connection that failed is one that is over.
        _ = serving.as_mut() => {}
        () = raised(&mut stopped) => {
            serving.as_mut().graceful_shutdown();
            let _ = serving.await;
        }
    }
}

/// Takes connections on `listener` and serves each with `router`, until
/// `stopped` says so. Then it takes no more, and returns once the ones
/// it has are closed.
pub(crate) async fn serve(
    listener: TcpListener,
    router: Router,
    mut stopped: watch::Receiver<bool>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = raised(&mut stopped) => break,
            // The ones that are over are let go of as they end.
            Some(_) = connections.join_next() => {}
            taken = listener.accept() => match taken {
                Ok((stream, _)) => {
                    connections.spawn(connection(stream, router.clone(), stopped.clone()));
                }
                Err(_) => tokio::time::sleep(TRY_AGAIN).await,
            },
        }
    }
    drop(listener);
    while connections.join_next().await.is_some() {}
}
