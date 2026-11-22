use std::path::PathBuf;

use super::{StoreError, record::ConversationKey};

#[derive(Clone, Debug, Default)]
pub(crate) struct LegacyInputs {
    pub(crate) wakes: Option<PathBuf>,
    pub(crate) journal: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct Conversion {
    pub(crate) conversations: Vec<ConversationKey>,
    pub(crate) wakes: usize,
    pub(crate) retained_wakes: Option<PathBuf>,
    pub(crate) follows: FollowConversion,
}

#[derive(Clone, Debug)]
pub(crate) enum FollowConversion {
    None,
    Retained { markers: usize, journal: PathBuf },
}

impl LegacyInputs {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn convert(
        &self,
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<Option<Conversion>, StoreError> {
        todo!()
    }
}
