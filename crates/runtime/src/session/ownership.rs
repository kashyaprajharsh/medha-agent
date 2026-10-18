use super::*;

pub(super) async fn open(
    log: &store::SqliteLog,
    state: &Path,
    resume: &Resume,
    notices: &dyn Notices,
) -> Result<(Session, Vec<Message>, SessionLease)> {
    Ok(match resolve_resume(log, state, resume, notices).await {
        Ok(Some((id, messages, lease))) => {
            notices.say(&format!(
                "resumed session {id} ({} prior messages)",
                messages.len()
            ));
            (
                Session {
                    id,
                    done: false,
                    autonomy: kernel::AutonomyLevel::Careful,
                },
                messages,
                lease,
            )
        }
        Ok(None) => {
            let session = Session::new();
            let lease = SessionLease::acquire(state, session.id)?;
            (session, Vec::new(), lease)
        }
        Err(error) if error.is::<LeaseFailure>() => return Err(error),
        Err(error) => {
            notices.say(&format!(
                "resume failed: {error} — starting a fresh session"
            ));
            let session = Session::new();
            let lease = SessionLease::acquire(state, session.id)?;
            (session, Vec::new(), lease)
        }
    })
}
