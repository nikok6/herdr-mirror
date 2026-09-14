//! Host picker placeholder until the Windows console/ConPTY data plane lands.

use tokio::runtime::Runtime;

use crate::util::{err, Env, Result};

pub const NO_LOCAL_FALLBACK_ENV: &str = "HERDR_MIRROR_NO_LOCAL_FALLBACK";

pub async fn summon(_env: Env) -> Result<()> {
    Err(err(
        "the interactive host picker is not supported on Windows yet",
    ))
}

pub fn menu(_rt: &Runtime, _env: Env) -> Result<()> {
    Err(err(
        "the interactive host picker is not supported on Windows yet",
    ))
}

pub async fn intercept(_env: Env, _what: &str) -> Result<()> {
    Ok(())
}
