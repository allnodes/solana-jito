use {
    super::Result,
    crate::admin_rpc_service,
    clap::{App, SubCommand},
    std::path::Path,
};

pub fn enable_experimental_feature_command<'a>() -> App<'a, 'a> {
    SubCommand::with_name("enable-experimental-feature").about("Enable experimental feature")
}

pub fn disable_experimental_feature_command<'a>() -> App<'a, 'a> {
    SubCommand::with_name("disable-experimental-feature").about("Disable experimental feature")
}

pub fn enable_experimental_feature_execute(ledger_path: &Path, enable: bool) -> Result<()> {
    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime().block_on(async move {
        admin_client
            .await?
            .enable_experimental_feature(enable)
            .await
    })?;
    Ok(())
}
