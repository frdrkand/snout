//! The small closed set of error codes that appear in snout's JSON contract,
//! and the mapping onto its four exit codes.
//!
//! The split is deliberate: the exit status stays coarse enough to be worth
//! branching on in a shell script, while the finer distinction lives in the
//! JSON `error.code` field where a program can read it.

use std::fmt;

/// The `error.code` alphabet. Serialised as its lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Code {
    /// The command line was wrong: an unknown subcommand, a flag that does not
    /// apply, two flags that contradict each other.
    Usage,
    /// snout is not set up: no flake path, an unknown reload component kind, an
    /// unparseable duration.
    Config,
    /// The command does not apply right now: nothing built to diff against, no
    /// session to reload into.
    Precondition,
    /// The check could not reach what it needed to.
    Network,
    /// nix ran and failed.
    Build,
    /// Anything snout itself got wrong.
    Internal,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Usage => "usage",
            Code::Config => "config",
            Code::Precondition => "precondition",
            Code::Network => "network",
            Code::Build => "build",
            Code::Internal => "internal",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Exit codes. Four, deliberately: 0 success, 1 the operation failed, 2 the
/// command line was wrong, 3 a precondition was not met.
pub const EXIT_OK: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_PRECONDITION: u8 = 3;

/// An error with a contract code attached.
///
/// `cause` carries the underlying failure so the message reads
/// `"<what snout was doing>: <what went wrong>"`, the same shape a wrapped
/// error produces.
#[derive(Debug)]
pub struct Error {
    pub code: Code,
    pub msg: String,
    pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    /// Builds a coded error.
    pub fn new(code: Code, msg: impl Into<String>) -> Self {
        Error {
            code,
            msg: msg.into(),
            cause: None,
        }
    }

    /// Attaches a code and a description to an existing error.
    pub fn wrap<E>(code: Code, cause: E, msg: impl Into<String>) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Error {
            code,
            msg: msg.into(),
            cause: Some(Box::new(cause)),
        }
    }

    /// The exit code this error maps onto.
    pub fn exit_code(&self) -> u8 {
        match self.code {
            Code::Usage | Code::Config => EXIT_USAGE,
            Code::Precondition => EXIT_PRECONDITION,
            _ => EXIT_FAILED,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.cause, self.msg.is_empty()) {
            (Some(cause), false) => write!(f, "{}: {}", self.msg, cause),
            (Some(cause), true) => write!(f, "{cause}"),
            (None, _) => f.write_str(&self.msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_ref().map(|e| e.as_ref() as _)
    }
}

/// The result type every fallible operation in snout returns.
pub type Result<T> = std::result::Result<T, Error>;

/// Shorthand constructors, one per code, so call sites read as prose.
macro_rules! constructors {
    ($($fn_name:ident => $code:expr),* $(,)?) => {
        impl Error {
            $(
                pub fn $fn_name(msg: impl Into<String>) -> Self {
                    Error::new($code, msg)
                }
            )*
        }
    };
}

constructors! {
    usage => Code::Usage,
    config => Code::Config,
    precondition => Code::Precondition,
    network => Code::Network,
    build => Code::Build,
    internal => Code::Internal,
}

/// Adds a code and a description to any error, for the `?` operator.
///
/// An error that reaches the top with no code at all would be reported as
/// `internal`, which is right but says nothing about what snout was attempting.
/// This is how a call site says that.
pub trait Context<T> {
    fn context(self, code: Code, msg: impl Into<String>) -> Result<T>;
    fn with_context<F, S>(self, code: Code, msg: F) -> Result<T>
    where
        F: FnOnce() -> S,
        S: Into<String>;
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context(self, code: Code, msg: impl Into<String>) -> Result<T> {
        self.map_err(|e| Error::wrap(code, e, msg))
    }

    fn with_context<F, S>(self, code: Code, msg: F) -> Result<T>
    where
        F: FnOnce() -> S,
        S: Into<String>,
    {
        self.map_err(|e| Error::wrap(code, e, msg()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_coarse_on_purpose() {
        // Config maps to 2 alongside Usage: from a script's point of view, "you
        // did not set this up" and "you typed it wrong" are the same class of
        // problem, and both mean retrying will not help.
        assert_eq!(Error::usage("x").exit_code(), EXIT_USAGE);
        assert_eq!(Error::config("x").exit_code(), EXIT_USAGE);
        assert_eq!(Error::precondition("x").exit_code(), EXIT_PRECONDITION);
        assert_eq!(Error::network("x").exit_code(), EXIT_FAILED);
        assert_eq!(Error::build("x").exit_code(), EXIT_FAILED);
        assert_eq!(Error::internal("x").exit_code(), EXIT_FAILED);
    }

    #[test]
    fn a_wrapped_error_reads_as_context_then_cause() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err = Error::wrap(Code::Internal, io, "reading the updated lock");
        assert_eq!(err.to_string(), "reading the updated lock: no such file");
    }

    #[test]
    fn an_unwrapped_error_is_just_its_message() {
        assert_eq!(
            Error::config("no flake path configured").to_string(),
            "no flake path configured"
        );
    }

    #[test]
    fn codes_serialise_as_the_contract_spells_them() {
        // These strings are the JSON contract; a renamed variant must not
        // silently rename the wire value.
        for (code, want) in [
            (Code::Usage, "usage"),
            (Code::Config, "config"),
            (Code::Precondition, "precondition"),
            (Code::Network, "network"),
            (Code::Build, "build"),
            (Code::Internal, "internal"),
        ] {
            assert_eq!(code.as_str(), want);
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{want}\""));
        }
    }

    #[test]
    fn context_attaches_a_code_to_a_foreign_error() {
        let r: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::other("disk went away"));
        let err = r.context(Code::Internal, "saving the answer").unwrap_err();
        assert_eq!(err.code, Code::Internal);
        assert_eq!(err.to_string(), "saving the answer: disk went away");
    }
}
