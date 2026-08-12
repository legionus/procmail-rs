#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLimitVariable {
    MessageSize,
    HeadersSize,
    BodySize,
    HeaderLineSize,
    HeaderFieldSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentTarget {
    Maildir,
    MessageLimit(MessageLimitVariable),
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableSource {
    RcFile,
    CommandLine,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariablePolicy {
    RcOnly(AssignmentTarget),
    RcOrCommandLine(AssignmentTarget),
    RuntimeOnly,
}

pub fn variable_policy(name: &str) -> VariablePolicy {
    match name {
        "MAILDIR" => VariablePolicy::RcOnly(AssignmentTarget::Maildir),
        "LASTFOLDER" => VariablePolicy::RuntimeOnly,
        "LIMIT_MSG_SIZE" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::MessageSize,
        )),
        "LIMIT_MSG_HEADERS" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::HeadersSize,
        )),
        "LIMIT_MSG_BODY" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::BodySize,
        )),
        "LIMIT_HEADER_LINE" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::HeaderLineSize,
        )),
        "LIMIT_HEADER_FIELD" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::HeaderFieldSize,
        )),
        _ => VariablePolicy::RcOrCommandLine(AssignmentTarget::User),
    }
}

impl VariablePolicy {
    pub fn allows(self, source: VariableSource) -> bool {
        matches!(
            (self, source),
            (Self::RcOnly(_), VariableSource::RcFile)
                | (
                    Self::RcOrCommandLine(_),
                    VariableSource::RcFile | VariableSource::CommandLine
                )
                | (Self::RuntimeOnly, VariableSource::Runtime)
        )
    }

    pub fn assignment_target(self, source: VariableSource) -> Option<AssignmentTarget> {
        match (self, source) {
            (Self::RcOnly(target), VariableSource::RcFile)
            | (
                Self::RcOrCommandLine(target),
                VariableSource::RcFile | VariableSource::CommandLine,
            ) => Some(target),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_explicit_sources_to_variable_classes() {
        assert_eq!(
            variable_policy("MAILDIR").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::Maildir)
        );
        assert!(!variable_policy("MAILDIR").allows(VariableSource::CommandLine));
        assert!(variable_policy("LASTFOLDER").allows(VariableSource::Runtime));
        assert!(!variable_policy("LASTFOLDER").allows(VariableSource::RcFile));
        assert_eq!(
            variable_policy("USER_VALUE").assignment_target(VariableSource::CommandLine),
            Some(AssignmentTarget::User)
        );
    }
}
