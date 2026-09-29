fn main() -> std::process::ExitCode {
    dekopon_shell::run_jq_worker_if_requested().unwrap_or(std::process::ExitCode::FAILURE)
}
