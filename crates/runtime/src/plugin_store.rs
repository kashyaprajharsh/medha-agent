//! Where plugins and their marketplaces are kept, the same for every surface.

use std::path::Path;

/// One construction for every surface, so CLI management and session startup
/// always read the same packages and pins.
pub fn store(medha_home: &Path, workspace: &Path, state: &Path) -> extensions::Store {
    extensions::Store::new(
        medha_home.join("plugins"),
        workspace.join(".medha").join("plugins"),
        medha_home.join("plugins.toml"),
        state.join("plugins.toml"),
        env!("CARGO_PKG_VERSION"),
    )
    .with_hook_sources(extensions::HookSource::standard(
        workspace,
        medha_home,
        dirs::home_dir().as_deref(),
    ))
}

pub fn marketplaces(medha_home: &Path) -> extensions::sources::Marketplaces {
    extensions::sources::Marketplaces::new(medha_home.join("plugin-marketplaces.toml"))
        .with_standard_defaults()
}
