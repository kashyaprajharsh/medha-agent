//! What a surface supplies to a chat: how to approve, how to ask, and who watches child agents.

use std::sync::Arc;

pub struct Surface {
    pub gate: Arc<dyn kernel::HumanGate>,
    pub asker: Arc<dyn kernel::Asker>,
    pub agents: crate::agents::AgentRoute,
}
