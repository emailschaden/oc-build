//! `grok models` subcommand.
// oc-build: opencode backend; lists the opencode server's models (`GET /api/model`).

use anyhow::{Context, Result};

use crate::acp::opencode_shim::OpencodeConfig;

pub async fn list_available_models(config: &OpencodeConfig) -> Result<()> {
    let client = oc_opencode_client::Client::new(config.url.clone(), config.password.clone())
        .context("failed to build the opencode client")?;
    let models = client.list_models().await.with_context(|| {
        format!(
            "couldn't list models from the opencode server at {}",
            config.url
        )
    })?;

    println!("Default model: {}", config.model);
    println!();
    println!("Available models:");
    for m in models {
        let id = format!("{}/{}", m.provider_id, m.id);
        if id == config.model {
            println!("  * {id} (default)");
        } else {
            println!("  - {id}");
        }
    }

    Ok(())
}
