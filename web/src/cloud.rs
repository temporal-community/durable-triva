//! Temporal Cloud connection settings for the operator-side binaries.
//!
//! Resolution is delegated to the SDK's own `envconfig` support so this project
//! behaves the way the `temporal` CLI does: a `temporal.toml` profile
//! underneath, `TEMPORAL_*` environment variables layered on top. The
//! repository's dotenv file is read first and folded into that environment
//! view, which is how the demo has always been configured.
//!
//! Both `temporal-trivia-web` and the `simulate_badges` helper used to carry
//! their own copy of this logic. They now share one, so the resolution rules
//! cannot drift apart again.

use std::{collections::HashMap, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, anyhow, bail};
use temporalio_client::{Client, ClientOptions, Connection, ConnectionOptions, TlsOptions};
use temporalio_common::envconfig::{
    ClientConfigProfile, LoadClientConfigProfileOptions, load_client_config_profile,
};
use temporalio_sdk_core::Url;

/// Environment variable naming an explicit dotenv file. When it is set the file
/// must exist: a typo has to fail loudly rather than silently starting the
/// controller with no Temporal settings at all. `firmware/build.rs` has always
/// asserted this, and the operator binaries now agree with it.
const ENV_FILE_VARIABLE: &str = "TEMPORAL_ENV_FILE";

/// Resolve a Temporal client profile from `temporal.toml`, the dotenv file, and
/// the process environment, in the SDK's documented precedence order.
pub fn load_profile() -> Result<ClientConfigProfile> {
    let mut environment = dotenv_values()?;
    // Real environment variables win over the dotenv file, preserving the
    // precedence these binaries have always had. Passing an explicit map means
    // the SDK reads only what we hand it, so the merge has to happen here.
    environment.extend(std::env::vars());

    // `ConfigError` boxes a bare `dyn Error`, which is not `Send + Sync`, so
    // anyhow cannot absorb it with `?`. Flatten it to a message instead.
    load_client_config_profile(
        LoadClientConfigProfileOptions::builder().build(),
        Some(&environment),
    )
    .map_err(|error| anyhow!("load Temporal client configuration: {error}"))
}

/// Read the repository's dotenv file, if one applies. Absent files are fine —
/// the environment or a `temporal.toml` profile may carry everything needed.
fn dotenv_values() -> Result<HashMap<String, String>> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project = manifest.parent().context("locate repository root")?;

    let path = match std::env::var_os(ENV_FILE_VARIABLE) {
        Some(explicit) => {
            let explicit = PathBuf::from(explicit);
            if !explicit.is_file() {
                bail!(
                    "{ENV_FILE_VARIABLE} points to missing file {}",
                    explicit.display()
                );
            }
            explicit
        }
        None => {
            let repo_env = project.join(".env");
            if repo_env.is_file() {
                repo_env
            } else {
                project.join(".env.temporal")
            }
        }
    };

    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read Temporal settings from {}", path.display()))?;
    temporal_trivia_shared::parse_env(&content)
        .with_context(|| format!("parse Temporal settings from {}", path.display()))
}

/// Connect a client using a profile from [`load_profile`].
pub async fn connect(profile: &ClientConfigProfile) -> Result<Client> {
    let address = required(profile.address.as_deref(), "TEMPORAL_ADDRESS")?;
    let namespace = required(profile.namespace.as_deref(), "TEMPORAL_NAMESPACE")?;
    let api_key = required(profile.api_key.as_deref(), "TEMPORAL_API_KEY")?;

    let target = if address.contains("://") {
        address.to_owned()
    } else {
        format!("https://{address}")
    };

    // An API key implies TLS, so the only reason to leave it unset is a profile
    // that disables it explicitly — which is how a local dev server is reached.
    let tls = match profile.tls.as_ref() {
        Some(tls) if tls.disabled == Some(true) => None,
        _ => Some(TlsOptions::default()),
    };

    let options = ConnectionOptions::new(Url::from_str(&target)?)
        .api_key(api_key)
        .maybe_tls_options(tls)
        .build();
    let connection = Connection::connect(options).await?;
    Client::new(connection, ClientOptions::new(namespace).build()).map_err(Into::into)
}

fn required<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str> {
    match value {
        Some(value) if !value.is_empty() => Ok(value),
        _ => bail!(
            "missing {name}; set it in the environment, .env.temporal, or a temporal.toml profile"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_rejects_absent_and_empty_values() {
        assert!(required(Some("ns.acct.tmprl.cloud:7233"), "TEMPORAL_ADDRESS").is_ok());
        assert!(required(Some(""), "TEMPORAL_ADDRESS").is_err());
        assert!(required(None, "TEMPORAL_ADDRESS").is_err());
    }

    #[test]
    fn an_explicit_env_file_that_does_not_exist_is_an_error() {
        // The whole point of the F2 fix: a typo in TEMPORAL_ENV_FILE must not
        // degrade into an empty settings map.
        unsafe { std::env::set_var(ENV_FILE_VARIABLE, "/nonexistent/temporal.env") };
        let error = dotenv_values().expect_err("missing explicit file must fail");
        unsafe { std::env::remove_var(ENV_FILE_VARIABLE) };
        assert!(error.to_string().contains("points to missing file"));
    }
}
