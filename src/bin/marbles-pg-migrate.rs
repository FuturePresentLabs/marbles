use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(about = "transactionally migrate hosted Marbles SQLite stores to PostgreSQL")]
struct Args {
    #[arg(long)]
    company_store_root: PathBuf,
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match marbles::postgres_migration::migrate_root(&args.company_store_root, &args.database_url) {
        Ok(reports) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&reports).expect("report is serializable")
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("migration failed: {error}");
            ExitCode::FAILURE
        }
    }
}
