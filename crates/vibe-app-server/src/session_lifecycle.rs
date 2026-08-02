use crate::release4::{Release4Error, Release4Service};

pub(crate) enum DeleteSessionError<E> {
    Prepare(Release4Error),
    Delete(E),
    Rollback { delete: E, rollback: Release4Error },
}

pub(crate) fn delete_session_transactionally<T, E>(
    release4: &Release4Service,
    session_id: &str,
    delete: impl FnOnce() -> Result<T, E>,
) -> Result<T, DeleteSessionError<E>> {
    let removal = release4
        .remove_session_transactional(session_id)
        .map_err(DeleteSessionError::Prepare)?;
    match delete() {
        Ok(result) => Ok(result),
        Err(delete) => match release4.restore_session(&removal) {
            Ok(()) => Err(DeleteSessionError::Delete(delete)),
            Err(rollback) => Err(DeleteSessionError::Rollback { delete, rollback }),
        },
    }
}
