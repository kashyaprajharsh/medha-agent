use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn resolve_turn_images(
        &self,
        cx: &RunContext<'_>,
        state: &mut RunState,
        request: &mut RequestState,
        candidate: &mut CompiledContext,
    ) -> Result<(), KernelError> {
        // Images either go to the model as pixels or reach it as a
        // described, clearly-labelled substitute. They are never
        // dropped on the way to a model that cannot see.
        if request.sending != Sending::AsText && self.route_reads_images()? {
            request.sent = crate::request_images::resolve_media(
                candidate,
                Arc::clone(&self.artifacts),
                request.sending,
                self.shrinker.as_ref(),
                self.image_side.load(Ordering::Relaxed),
            )
            .await;
            if request.sending != Sending::OneSmall && request.sent.retired > state.retired_said {
                state.retired_said = request.sent.retired;
                cx.sink.notice(&format!(
                    "{} older image(s) from tools are no longer sent to the model, to \
                                 keep requests within limits. The model was told.",
                    request.sent.retired
                ));
            }
            if request.sent.unreadable > 0 && !state.unreadable_said {
                state.unreadable_said = true;
                cx.sink.notice(&format!(
                    "{} image(s) could not be read back and were left out. The model \
                                 was told.",
                    request.sent.unreadable
                ));
            }
        } else {
            let done = crate::vision::describe_media(
                candidate,
                Arc::clone(&self.artifacts),
                self.vision.as_deref(),
            )
            .await;
            if let (Some(describer), true) = (&self.vision, done.described > 0) {
                cx.sink.notice(&format!(
                    "{} image(s) described by {} — this model has no image input",
                    done.described,
                    describer.model()
                ));
            }
            // Said once a run: every request of it carries the same images.
            if done.not_shown > 0 && !state.images_left_out_said {
                state.images_left_out_said = true;
                cx.sink.notice(&match (&self.vision, &done.failure) {
                    (Some(describer), Some(error)) => format!(
                        "{} image(s) left out: this model cannot read images and {} \
                                     could not describe them ({error}). The model was told.",
                        done.not_shown,
                        describer.model()
                    ),
                    _ => format!(
                        "{} image(s) left out: this model cannot read images. The \
                                     model was told. To have images described, name a vision \
                                     model under [auxiliary] vision in config.toml.",
                        done.not_shown
                    ),
                });
            }
        }
        Ok(())
    }
}
