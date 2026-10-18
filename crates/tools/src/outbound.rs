use super::*;

pub(super) enum Selected {
    Tavily(String),
    Brave(String),
    Searxng(String),
}

pub(super) fn selected(settings: &SearchSettings) -> Result<Option<Selected>, String> {
    if let Some(error) = &settings.configuration_error {
        return Err(error.clone());
    }
    Ok(Some(match settings.provider {
        SearchProvider::DuckDuckGo => return Ok(None),
        SearchProvider::Tavily => Selected::Tavily(
            settings
                .tavily_key()
                .ok_or("tavily: no API key configured")?,
        ),
        SearchProvider::Brave => {
            Selected::Brave(settings.brave_key().ok_or("brave: no API key configured")?)
        }
        SearchProvider::Searxng => Selected::Searxng(
            settings
                .searxng_url()
                .ok_or("searxng: no instance URL configured")?,
        ),
    }))
}

impl Selected {
    pub fn destination(&self) -> String {
        match self {
            Self::Tavily(_) => "https://api.tavily.com/search".into(),
            Self::Brave(_) => "https://api.search.brave.com/res/v1/web/search".into(),
            Self::Searxng(url) => format!("{}/search", url.trim_end_matches('/')),
        }
    }
    pub async fn run(
        &self,
        client: &reqwest::Client,
        query: &str,
        max: usize,
    ) -> Result<Value, ToolError> {
        match self {
            Self::Tavily(key) => tavily_search(client, query, max, key).await,
            Self::Brave(key) => brave_search(client, query, max, key).await,
            Self::Searxng(url) => searxng_search(client, query, max, url).await,
        }
    }
}

#[cfg(test)]
#[path = "outbound_tests.rs"]
mod tests;
