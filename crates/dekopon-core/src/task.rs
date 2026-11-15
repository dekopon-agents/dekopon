use std::{any::Any, convert::Infallible, error::Error, fmt, io::Write as _};

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map_or("unknown", std::panic::Location::file);
        let line = info.location().map_or(0, std::panic::Location::line);
        let _written = writeln!(
            std::io::stderr(),
            "panic class=panicked location={location}:{line}"
        );
    }));
}

#[derive(Debug)]
pub enum TaskOutcome<E = Infallible> {
    Completed,
    Failed(E),
    Panicked,
    Cancelled,
}

impl<E> TaskOutcome<E> {
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Panicked => "panicked",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_result(result: Result<(), E>) -> Self {
        match result {
            Ok(()) => Self::Completed,
            Err(error) => Self::Failed(error),
        }
    }
}

#[cfg(feature = "tokio-task")]
impl TaskOutcome {
    pub fn from_join_error(error: tokio::task::JoinError) -> Self {
        error.into()
    }
}

#[cfg(feature = "tokio-task")]
impl<E> TaskOutcome<E> {
    pub fn from_join(result: Result<Result<(), E>, tokio::task::JoinError>) -> Self {
        match result {
            Ok(result) => Self::from_result(result),
            Err(error) => error.into(),
        }
    }
}

#[cfg(feature = "tokio-task")]
impl<E> From<tokio::task::JoinError> for TaskOutcome<E> {
    fn from(error: tokio::task::JoinError) -> Self {
        if error.is_panic() {
            Self::Panicked
        } else {
            Self::Cancelled
        }
    }
}

impl<E: fmt::Display> fmt::Display for TaskOutcome<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed => formatter.write_str("task completed"),
            Self::Failed(error) => write!(formatter, "task failed: {error}"),
            Self::Panicked => formatter.write_str("task panicked"),
            Self::Cancelled => formatter.write_str("task cancelled"),
        }
    }
}

impl<E: Error + 'static> Error for TaskOutcome<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed(error) => Some(error),
            Self::Completed | Self::Panicked | Self::Cancelled => None,
        }
    }
}

impl<E> From<Box<dyn Any + Send>> for TaskOutcome<E> {
    fn from(_payload: Box<dyn Any + Send>) -> Self {
        Self::Panicked
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::install_panic_hook;

    #[test]
    fn panic_hook_prints_location_and_class_without_payload() {
        const TOKEN: &str = "fixture-secret-0123456789";
        if std::env::var_os("DEKOPON_TEST_PANIC_HOOK_CHILD").is_some() {
            install_panic_hook();
            assert!(std::panic::catch_unwind(|| panic!("{TOKEN}")).is_err());
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "task::tests::panic_hook_prints_location_and_class_without_payload",
                "--nocapture",
            ])
            .env("DEKOPON_TEST_PANIC_HOOK_CHILD", "1")
            .output()
            .unwrap();
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("panic class=panicked location="),
            "{stderr}"
        );
        assert!(!stderr.contains(TOKEN), "{stderr}");
    }
}
