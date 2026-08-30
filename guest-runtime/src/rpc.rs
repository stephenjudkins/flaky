use std::future::Future;
use std::io;
use std::pin::Pin;

use apis::tarpc::client::RequestDispatch;
use apis::tarpc::serde_transport::Transport;
use apis::tarpc::server::Channel;
use apis::tarpc::{ClientMessage, Response};
use apis::{
    GuestApi, GuestApiRequest, GuestApiResponse, HOST_API_PORT, HostApiRequest, HostApiResponse,
    Postcard,
};
use futures::prelude::*;
use tokio_vsock::{VMADDR_CID_HOST, VsockAddr, VsockStream};

pub type ByteStream = VsockStream;

pub async fn connect(port: u32) -> io::Result<ByteStream> {
    VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, port)).await
}

type HostApiCodec = Postcard<Response<HostApiResponse>, ClientMessage<HostApiRequest>>;
pub type HostApiTransport =
    Transport<ByteStream, Response<HostApiResponse>, ClientMessage<HostApiRequest>, HostApiCodec>;

pub type HostApiClient =
    apis::HostApiClient<apis::tarpc::client::Channel<HostApiRequest, HostApiResponse>>;

pub struct HostApiConnection {
    client: HostApiClient,
    dispatch: Pin<Box<RequestDispatch<HostApiRequest, HostApiResponse, HostApiTransport>>>,
}

impl HostApiConnection {
    pub async fn connect() -> io::Result<Self> {
        let stream = connect(HOST_API_PORT).await?;
        let transport: HostApiTransport = Transport::from((stream, Postcard::default()));
        let apis::tarpc::client::NewClient { client, dispatch } =
            apis::HostApiClient::new(apis::tarpc::client::Config::default(), transport);
        Ok(HostApiConnection {
            client,
            dispatch: Box::pin(dispatch),
        })
    }

    pub async fn drive<T, Fut>(&mut self, f: impl FnOnce(HostApiClient) -> Fut) -> io::Result<T>
    where
        Fut: Future<Output = io::Result<T>>,
    {
        let fut = f(self.client.clone());
        tokio::select! {
            result = fut => result,
            _ = &mut self.dispatch => Err(io::Error::new(io::ErrorKind::BrokenPipe, "dispatch terminated")),
        }
    }
}

pub fn serve_guest_api<S: GuestApi + Clone>(
    stream: ByteStream,
    server: S,
) -> impl Future<Output = ()> {
    async move {
        let transport: Transport<
            ByteStream,
            ClientMessage<GuestApiRequest>,
            Response<GuestApiResponse>,
            Postcard<ClientMessage<GuestApiRequest>, Response<GuestApiResponse>>,
        > = Transport::from((stream, Postcard::default()));
        let channel = apis::tarpc::server::BaseChannel::with_defaults(transport);
        channel
            .execute(server.serve())
            .for_each(|resp| async {
                let _ = resp.await;
            })
            .await;
    }
}
