#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecoveryOutcome {
    pub interrupted: bool,
    pub interrupted_run_id: Option<String>,
}
