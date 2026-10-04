//! Runs the configured verification command after edits.

use std::sync::Arc;

/// Keep the verifier's diagnostic tail bounded.
const VERIFY_MAX_OUTPUT: usize = 8_192;

pub struct CommandVerifier {
    pub command: String,
    pub required: bool,
    pub dir: std::path::PathBuf,
    pub limit: std::time::Duration,
    pub exec: Arc<dyn sandbox::ExecBackend>,
}

#[async_trait::async_trait]
impl kernel::Verifier for CommandVerifier {
    fn required(&self) -> bool {
        self.required
    }

    async fn check(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<kernel::VerifyReport> {
        let out = match sandbox::run_shell_bounded_with(
            self.exec.as_ref(),
            &self.command,
            &self.dir,
            self.limit,
            VERIFY_MAX_OUTPUT,
            Some(cancel),
        )
        .await
        {
            Ok(out) => out,
            // A configured verifier fails closed when it cannot start.
            Err(error) => {
                return Some(kernel::VerifyReport {
                    ok: false,
                    summary: format!("could not run `{}`", self.command),
                    output: error.to_string(),
                });
            }
        };
        Some(kernel::VerifyReport {
            ok: out.passed(),
            summary: match (out.cancelled, out.timed_out) {
                (true, _) => format!("`{}` cancelled", self.command),
                (_, true) => format!("`{}` timed out", self.command),
                _ => format!("`{}` exit {}", self.command, out.status.unwrap_or(-1)),
            },
            output: out.output,
        })
    }
}
