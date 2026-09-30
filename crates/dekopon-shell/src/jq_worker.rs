use std::{
    path::PathBuf,
    process::{Child, ExitCode},
    sync::OnceLock,
};

use jaq_json::Val;

pub const JQ_WORKER_MARKER: &str = "DEKOPON_SHELL_JQ_WORKER";
static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_jq_worker_executable(path: PathBuf) -> Result<(), PathBuf> {
    EXECUTABLE.set(path)
}

pub(crate) fn executable() -> Option<&'static PathBuf> {
    EXECUTABLE.get()
}

pub fn run_jq_worker_if_requested() -> Option<ExitCode> {
    std::env::var_os(JQ_WORKER_MARKER)?;
    Some(match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(super::builtins::jq::WorkerFailure::InvalidInput(message)) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
        Err(super::builtins::jq::WorkerFailure::Failed(message)) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    })
}

fn serve() -> Result<(), super::builtins::jq::WorkerFailure> {
    use super::builtins::jq::WorkerFailure;
    #[cfg(target_os = "linux")]
    limit_address_space().map_err(WorkerFailure::Failed)?;

    let mut stdin = std::io::stdin().lock();
    let mut header = String::new();
    std::io::BufRead::read_line(&mut stdin, &mut header)
        .map_err(|error| WorkerFailure::Failed(format!("jq: invalid worker request: {error}")))?;
    let (filter, slurp): (String, bool) = serde_json::from_str(&header)
        .map_err(|error| WorkerFailure::Failed(format!("jq: invalid worker request: {error}")))?;
    let documents = serde_json::Deserializer::from_reader(stdin).into_iter::<Val>();
    let input: Box<dyn Iterator<Item = Result<Val, serde_json::Error>>> = if slurp {
        Box::new(documents.take(1))
    } else {
        Box::new(documents)
    };
    super::builtins::jq::run_filter(&filter, input, &mut std::io::stdout().lock())
}

#[cfg(target_os = "linux")]
fn limit_address_space() -> Result<(), String> {
    use rustix::process::{Resource, Rlimit, setrlimit};

    const HEADROOM: u64 = 256 * 1024 * 1024;
    std::fs::write("/proc/self/oom_score_adj", "1000")
        .map_err(|error| format!("jq: could not adjust worker OOM score: {error}"))?;
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("jq: could not read worker address space: {error}"))?;
    let baseline = status
        .lines()
        .find_map(|line| line.strip_prefix("VmSize:").map(str::trim))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|kib| kib.checked_mul(1024))
        .ok_or_else(|| "jq: could not parse worker VmSize".to_owned())?;
    let ceiling = baseline
        .checked_add(HEADROOM)
        .ok_or_else(|| "jq: worker address space limit overflowed".to_owned())?;
    setrlimit(
        Resource::As,
        Rlimit {
            current: Some(ceiling),
            maximum: Some(ceiling),
        },
    )
    .map_err(|error| format!("jq: could not limit worker address space: {error}"))
}

pub(crate) struct WorkerChild(pub(crate) Child);

impl Drop for WorkerChild {
    fn drop(&mut self) {
        let _kill_result = self.0.kill();
        let _wait_result = self.0.wait();
    }
}
