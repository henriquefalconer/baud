use anyhow::Result;
use crate::{client::Client, fmt};

pub async fn run(c: &Client, json: bool) -> Result<()> {
    let v = c.get("/budget").await?;
    fmt::print(&v, json);
    Ok(())
}
