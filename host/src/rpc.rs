use std::future::Future;
use std::pin::Pin;

use apis::tarpc::client::RequestDispatch;
use apis::tarpc::serde_transport::Transport;
use apis::tarpc::server::Channel;
use apis::tarpc::{ClientMessage, Response};
use apis::{GuestApiRequest, GuestApiResponse, HostApi, HostApiRequest, HostApiResponse, Postcard};
use futures::prelude::*;

pub type ByteStream = tokio::net::UnixStream;

type GuestApiCodec = Postcard<Response<GuestApiResponse>, ClientMessage<GuestApiRequest>>;
pub type GuestApiTransport = Transport<
    ByteStream,
    Response<GuestApiResponse>,
    ClientMessage<GuestApiRequest>,
    GuestApiCodec,
>;

pub type GuestApiClient =
    apis::GuestApiClient<apis::tarpc::client::Channel<GuestApiRequest, GuestApiResponse>>;

pub struct GuestApiConnection {
    client: GuestApiClient,
    dispatch: Pin<Box<RequestDispatch<GuestApiRequest, GuestApiResponse, GuestApiTransport>>>,
}

impl GuestApiConnection {
    pub fn new(stream: ByteStream) -> Self {
        let transport: GuestApiTransport = Transport::from((stream, Postcard::default()));
        let apis::tarpc::client::NewClient { client, dispatch } =
            apis::GuestApiClient::new(apis::tarpc::client::Config::default(), transport);
        GuestApiConnection {
            client,
            dispatch: Box::pin(dispatch),
        }
    }

    pub async fn drive<T, Fut>(
        &mut self,
        f: impl FnOnce(GuestApiClient) -> Fut,
    ) -> anyhow::Result<T>
    where
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let fut = f(self.client.clone());
        tokio::select! {
            result = fut => result,
            _ = &mut self.dispatch => anyhow::bail!("dispatch terminated"),
        }
    }
}

pub fn serve_host_api<S: HostApi + Clone>(
    stream: ByteStream,
    server: S,
) -> impl Future<Output = ()> {
    async move {
        let transport: Transport<
            ByteStream,
            ClientMessage<HostApiRequest>,
            Response<HostApiResponse>,
            Postcard<ClientMessage<HostApiRequest>, Response<HostApiResponse>>,
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
