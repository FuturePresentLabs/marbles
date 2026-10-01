use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;

#[derive(Parser)]
#[command(about = "continuously prove Marbles health and authenticated API availability")]
struct Args {
    #[arg(long, env = "MARBLES_URL")]
    url: String,
    #[arg(long, env = "MARBLES_TOKEN", hide_env_values = true)]
    token: String,
    #[arg(long, default_value_t = 600)]
    duration_seconds: u64,
    #[arg(long, default_value_t = 1)]
    interval_seconds: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("building HTTP client: {error}");
            return ExitCode::FAILURE;
        }
    };
    let base = args.url.trim_end_matches('/');
    let deadline = Instant::now() + Duration::from_secs(args.duration_seconds);
    let mut samples = 0_u64;
    let mut failures = 0_u64;
    while Instant::now() < deadline {
        samples += 1;
        let health = client.get(format!("{base}/healthz")).send().await;
        let authenticated = client
            .post(format!("{base}/v1/projects.list"))
            .bearer_auth(&args.token)
            .json(&serde_json::json!({}))
            .send()
            .await;
        let health_status = health.as_ref().ok().map(reqwest::Response::status);
        let auth_status = authenticated.as_ref().ok().map(reqwest::Response::status);
        if health_status != Some(reqwest::StatusCode::OK)
            || auth_status != Some(reqwest::StatusCode::OK)
        {
            failures += 1;
            eprintln!(
                "probe failure sample={samples} health={} authenticated={}",
                health_status
                    .map(|status| status.as_u16().to_string())
                    .unwrap_or_else(|| health.err().expect("missing error").to_string()),
                auth_status
                    .map(|status| status.as_u16().to_string())
                    .unwrap_or_else(|| authenticated.err().expect("missing error").to_string()),
            );
        }
        tokio::time::sleep(Duration::from_secs(args.interval_seconds)).await;
    }
    println!("samples={samples} failures={failures}");
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
