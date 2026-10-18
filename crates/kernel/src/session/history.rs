use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn save_feedback(
        &self,
        history: &mut History,
        event: Event,
        text: impl Into<String>,
    ) -> Result<(), KernelError> {
        self.log.append(event).await?;
        let message = Message::user(text).carrying(TrustLabel::Tool);
        history.ordered_messages.push(message.ordered());
        history.messages.push(message);
        Ok(())
    }
}
