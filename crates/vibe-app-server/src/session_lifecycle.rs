use crate::projects::{ProjectsService, ProjectsServiceError};

pub(crate) enum DeleteSessionError<E> {
    Prepare(ProjectsServiceError),
    Delete(E),
    Rollback {
        delete: E,
        rollback: ProjectsServiceError,
    },
}

pub(crate) fn delete_session_transactionally<T, E>(
    projects: &ProjectsService,
    session_id: &str,
    delete: impl FnOnce() -> Result<T, E>,
) -> Result<T, DeleteSessionError<E>> {
    let removal = projects
        .remove_session_transactional(session_id)
        .map_err(DeleteSessionError::Prepare)?;
    match delete() {
        Ok(result) => Ok(result),
        Err(delete) => match projects.restore_session(&removal) {
            Ok(()) => Err(DeleteSessionError::Delete(delete)),
            Err(rollback) => Err(DeleteSessionError::Rollback { delete, rollback }),
        },
    }
}
