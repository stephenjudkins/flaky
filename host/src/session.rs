use apis::tarpc::context;

use crate::rpc::GuestApiConnection;

pub async fn run_session(guest_api: &mut GuestApiConnection) -> anyhow::Result<()> {
    guest_api
        .drive(|c| async move {
            let resp = c.hello(context::current(), "world".to_string()).await?;
            println!("host: guest replied: {resp}");

            let resp = c.shutdown(context::current()).await?;
            println!("host: guest replied: {resp}");
            Ok(())
        })
        .await
}
