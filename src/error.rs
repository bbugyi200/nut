use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitClass {
    Operational,
    Usage,
}

impl ExitClass {
    pub const fn code(self) -> i32 {
        match self {
            Self::Operational => 1,
            Self::Usage => 2,
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct NutError {
    class: ExitClass,
    message: String,
    hint: Option<String>,
}

impl NutError {
    pub fn operational(message: impl Into<String>) -> Self {
        Self {
            class: ExitClass::Operational,
            message: message.into(),
            hint: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            class: ExitClass::Usage,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    pub fn exit_code(&self) -> i32 {
        self.class.code()
    }
}

impl From<crate::host::HostError> for NutError {
    fn from(error: crate::host::HostError) -> Self {
        Self::operational(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_failures_have_exit_code_one() {
        assert_eq!(NutError::operational("offline").exit_code(), 1);
        assert_eq!(NutError::usage("bad syntax").exit_code(), 2);
    }
}
