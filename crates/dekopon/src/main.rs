use clap::Parser;
use dekopon::cli::Cli;

fn main() {
    let cli = Cli::parse();
    std::process::exit(dekopon::run(cli));
}
