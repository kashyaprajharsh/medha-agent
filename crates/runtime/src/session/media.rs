use super::*;

pub(super) struct Admitted {
    pub prompt: String,
    pub attached_images: Vec<kernel::MediaPart>,
    pub auxiliary_vision: Option<Arc<vision::AuxiliaryVision>>,
}

pub(super) async fn admit(
    setup: &Setup<'_>,
    prompt: String,
    model_profiles: &Arc<std::sync::Mutex<config::Config>>,
    artifacts: Arc<dyn kernel::ArtifactStore>,
) -> Result<Admitted> {
    let Setup {
        options,
        cwd,
        notices,
        ..
    } = *setup;
    // A model with no image input is not a dead end when an auxiliary vision
    // profile is configured: the kernel has it describe the image instead.
    let configured_vision = {
        let configured = model_profiles.lock().unwrap().clone();
        configured
            .auxiliary
            .vision
            .clone()
            .filter(|name| !name.trim().is_empty())
            .map(|name| (name, configured))
    };
    let auxiliary_vision = configured_vision.and_then(|(name, configured)| {
        match vision::connect(&configured, &name, Some(&options.model_env)) {
            Ok(provider) => {
                notices.say(&format!(
                    "auxiliary vision: '{name}' reads images this model cannot"
                ));
                Some(Arc::new(vision::AuxiliaryVision::new(provider)))
            }
            Err(error) => {
                notices.say(&format!(
                    "auxiliary vision profile '{name}' unavailable: {error:#}"
                ));
                None
            }
        }
    });

    // `--attach` is explicit; a path written into the prompt attaches too, so
    // `medha "explain ./shot.png"` behaves the way it reads.
    let mut attach_paths = options.attach.clone();
    let scanned = attachments::refs::scan(&prompt, cwd);
    for path in &scanned {
        if !attach_paths.contains(path) {
            attach_paths.push(path.clone());
        }
    }
    // An unreachable path carries no meaning once its image is attached.
    let prompt = match attachments::refs::strip_unreachable(&prompt, &scanned, cwd) {
        stripped if stripped.trim().is_empty() && !attach_paths.is_empty() => {
            attachments::IMAGE_ONLY_PROMPT.to_string()
        }
        stripped => stripped,
    };
    let attached_images: Vec<_> = attachments::ingest(attach_paths, artifacts.clone())
        .await?
        .into_iter()
        .inspect(|image| {
            notices.say(&format!(
                "attached {}{}",
                image.summary(),
                image
                    .note
                    .as_deref()
                    .map(|note| format!(" ({note})"))
                    .unwrap_or_default()
            ))
        })
        .map(|image| image.part)
        .collect();
    Ok(Admitted {
        prompt,
        attached_images,
        auxiliary_vision,
    })
}
