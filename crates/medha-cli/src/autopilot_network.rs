//! Turning Autopilot on asks once whether the chat may use the network, and says plainly what Autopilot may allow.

use std::sync::Arc;

use crate::chat::Writer;

pub(crate) const ASK: &str = "open the network for this chat in Autopilot";
pub(crate) const DETAIL: &str = "In Autopilot, Medha runs edits and commands without asking, and a reviewer may allow \
     network access or folders outside the project, one action at a time.\n\n\
     Allow: commands in this chat may use the network until the chat ends.\n\
     Deny: each command that needs the network goes to the reviewer, and to you when it is unsure.";

/// Asks once, beside the chat so the chat stays free to take the answer; no one is asked when no one can answer.
pub(crate) fn offer(
    gate: Arc<dyn kernel::HumanGate>,
    sandbox: Arc<sandbox::WorkspaceSandbox>,
    writer: Arc<Writer>,
) {
    let permissions = sandbox.permission_manager();
    // Where the sandbox never closes the network, as on Windows, there is nothing to open.
    if gate.answerer() == kernel::Decider::Nobody
        || permissions.network_grant().granted()
        || !sandbox.denies_network()
    {
        return;
    }
    tokio::spawn(async move {
        let notice = if gate.confirm(ASK, Some(DETAIL), false).await.approved() {
            match permissions.open_network("approved when Autopilot was turned on") {
                Ok(()) => "The network is open for this chat.",
                Err(_) => {
                    "The network could not be opened for this chat, so the reviewer decides each command."
                }
            }
        } else {
            "The network stays closed. Autopilot's reviewer decides each command that needs it."
        };
        writer.notify_params(
            "event",
            &protocol::TurnEvent::Notice {
                text: notice.into(),
            },
        );
    });
}
