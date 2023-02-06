mod proxy;

use azure_identity::{
    // AzureCliCredential,
    ClientSecretCredential,
    TokenCredentialOptions,
};
use clap::Parser;
use tokio::io;
use azure_mgmt_compute::Client as ComputeClient;
use std::sync::Arc;

/// TCP proxy that starts the azure server before connecting
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(long)]
    azure_client_id: String,

    #[arg(long)]
    azure_client_secret: String,

    #[arg(long)]
    azure_tenant_id: String,

    #[arg(long)]
    azure_subscription_id: String,

    #[arg(long)]
    azure_resource_group: String,

    #[arg(long)]
    azure_vm_name: String,

    /// The address of the client that we will be proxying traffic from
    #[arg(short, long, value_name = "ADDRESS")]
    client: String,

    /// The address of the origin that we will be proxying traffic to 
    #[arg(short, long, value_name = "ADDRESS")]
    server: String,

    /// The time in seconds to wait before stopping the server
    #[arg(short, long, default_value = "300")]
    timeout: f64,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    let http_client = azure_core::new_http_client();

    let credential = Arc::new(ClientSecretCredential::new(
        http_client.clone(),
        args.azure_tenant_id.clone(),
        args.azure_client_id.clone(),
        args.azure_client_secret.clone(),
        TokenCredentialOptions::default(),
    ));

    // let credential = Arc::new(AzureCliCredential::new());

    let compute_client = ComputeClient::builder(credential.clone()).build();
    proxy::start_proxy(args, compute_client).await
}
