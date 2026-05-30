use std::env;
use whisperer::{controller::run, server::serve as server, telemetry};

// Ensure that we have a valid port
fn port(var: &str) -> u16 {
    env::var(var)
        .unwrap_or_else(|_| panic!("{var} not defined"))
        .parse::<u16>()
        .unwrap_or_else(|_| panic!("{var} not a valid port"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let healthcheck_port = port("HEALTHCHECK_PORT");
    let telemetry = telemetry::init()?;
    let controller = run(telemetry.metrics.clone());
    let server = server(healthcheck_port);
    tokio::join!(controller, server).1?;
    telemetry.shutdown()?;
    Ok(())
}
