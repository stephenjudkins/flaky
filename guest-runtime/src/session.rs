use std::io::{self, Write};

use apis::tarpc::context;

use crate::guest_api::GuestApiServer;
use crate::poweroff;
use crate::rpc::{self, HostApiConnection};

pub async fn run_session() -> io::Result<()> {
    let mut out = std::io::stdout();
    let server = GuestApiServer::new();

    let mut guest_api = Some(rpc::connect(apis::GUEST_API_PORT).await?);

    let mut host_api = HostApiConnection::connect().await?;
    let _ = writeln!(
        out,
        "guest: connected to host on vsock port {}",
        apis::HOST_API_PORT
    );
    let resp = host_api
        .drive(|c| async move {
            c.greet(context::current(), "guest".to_string())
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        })
        .await?;
    let _ = writeln!(out, "guest: host replied: {resp}");

    loop {
        let stream = match guest_api.take() {
            Some(stream) => stream,
            None => rpc::connect(apis::GUEST_API_PORT).await?,
        };
        let _ = writeln!(
            out,
            "guest: connected to host on vsock port {}",
            apis::GUEST_API_PORT
        );
        rpc::serve_guest_api(stream, server.clone()).await;
        let _ = writeln!(out, "guest: connection closed");
        if server.shutdown_requested() {
            poweroff();
        }
    }
}
