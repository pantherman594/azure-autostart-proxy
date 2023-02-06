# Azure Autostart Proxy

## Usage

1. Go to portal.azure.com and open the cloud shell
2. Run `az account show --query id -o tsv` to get your subscription ID.
3. Run `az ad sp create-for-rbac --role Contributor --scopes /subscriptions/<subscription id>` and take note of the `appId`, `password`, and `tenant`.
4. Run `cargo run -- --azure-client-id <app id> --azure-client-secret <password> --azure-tenant-id <tenant> --azure-subscription-id <subscription id> --azure-resource-group <resource group> --azure-vm-name <vm name> --client 0.0.0.0:<port> --server <external server ip>:<external port> --timeout <timeout>`
