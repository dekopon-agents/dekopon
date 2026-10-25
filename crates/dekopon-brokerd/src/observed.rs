use std::process::ExitCode;

use dekopon_core::error_chain;
use dekopon_telemetry::Install;

pub(crate) fn finish(
    runtime: tokio::runtime::Runtime,
    work: impl Future<Output = ExitCode>,
) -> ExitCode {
    let code = runtime.block_on(work);
    // Blocking work still running here outlived the shutdown deadline; it ends with the process.
    runtime.shutdown_background();
    code
}

pub(crate) async fn observed<E: std::error::Error>(
    install: Install,
    work: impl Future<Output = Result<(), E>>,
    deadline: Option<dekopon_brokerd::ShutdownDeadline>,
) -> ExitCode {
    let telemetry = match install.install() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("dekopon-brokerd: could not install tracing subscriber: {error}");
            return ExitCode::FAILURE;
        }
    };
    let code = match work.await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if deadline.is_none() {
                tracing::error!(event = "broker_exit", error = %error_chain(&error), "broker.exit");
            } else {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "level": "ERROR",
                        "event": "broker_exit",
                        "error": error_chain(&error),
                        "message": "broker.exit",
                        "target": "dekopon_brokerd",
                    })
                );
            }
            ExitCode::FAILURE
        }
    };
    let flushed = match deadline
        .as_ref()
        .and_then(dekopon_brokerd::ShutdownDeadline::get)
    {
        None => telemetry.shutdown(),
        Some(deadline) if tokio::time::Instant::now() >= deadline => {
            eprintln!("dekopon-brokerd: telemetry flush abandoned at the shutdown deadline");
            return code;
        }
        Some(deadline) => {
            let flush = tokio::task::spawn_blocking(move || telemetry.shutdown());
            match tokio::time::timeout_at(deadline, flush).await {
                Ok(Ok(flushed)) => flushed,
                Ok(Err(error)) => {
                    eprintln!("dekopon-brokerd: telemetry flush failed: {error}");
                    return code;
                }
                Err(_) => {
                    eprintln!(
                        "dekopon-brokerd: telemetry flush abandoned at the shutdown deadline"
                    );
                    return code;
                }
            }
        }
    };
    if let Err(error) = flushed {
        tracing::error!(
            event = "broker_telemetry_shutdown_failed",
            error = %error,
            "broker.telemetry.shutdown.failed"
        );
    }
    code
}
