#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExitCode(u8);

impl ExitCode {
    pub const SUCCESS: Self = Self(0);
    pub const FAILURE: Self = Self(1);
    pub const SYNTAX: Self = Self(2);
    pub const TIMEOUT: Self = Self(124);
    pub const CANCELLED: Self = Self(130);
    pub const DENIED: Self = Self(126);
    pub const NOT_FOUND: Self = Self(127);

    #[must_use]
    pub fn from_script_exit(status: i64) -> Self {
        Self(u8::try_from(status.rem_euclid(256)).unwrap_or(0))
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<ExitCode> for u8 {
    fn from(code: ExitCode) -> Self {
        code.0
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

impl std::fmt::Display for ExitCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}
