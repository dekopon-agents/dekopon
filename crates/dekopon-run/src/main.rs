use clap::Parser;
use dekopon_run::cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    std::process::exit(dekopon_run::run(cli).await);
}
