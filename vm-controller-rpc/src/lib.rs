pub use tarpc;
pub use tokio_serde;

use std::io;
use std::marker::PhantomData;
use std::pin::Pin;

use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use tokio_serde::{Deserializer, Serializer};

#[derive(Debug)]
pub struct Postcard<Item, SinkItem> {
    ghost: PhantomData<(Item, SinkItem)>,
}

impl<Item, SinkItem> Default for Postcard<Item, SinkItem> {
    fn default() -> Self {
        Postcard { ghost: PhantomData }
    }
}

impl<Item, SinkItem> Serializer<SinkItem> for Postcard<Item, SinkItem>
where
    SinkItem: Serialize,
{
    type Error = io::Error;

    fn serialize(self: Pin<&mut Self>, item: &SinkItem) -> Result<Bytes, Self::Error> {
        postcard::to_allocvec(item)
            .map(Bytes::from)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl<Item, SinkItem> Deserializer<Item> for Postcard<Item, SinkItem>
where
    Item: for<'a> Deserialize<'a>,
{
    type Error = io::Error;

    fn deserialize(self: Pin<&mut Self>, src: &BytesMut) -> Result<Item, Self::Error> {
        postcard::from_bytes(src).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

#[tarpc::service]
pub trait VmController {
    async fn hello(x: String) -> String;
    async fn shutdown() -> String;
}
