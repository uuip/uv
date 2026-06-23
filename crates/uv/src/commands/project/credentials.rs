use anyhow::Result;
use uv_client::BaseClientBuilder;
use uv_pep508::VersionOrUrl;
use uv_pypi_types::{ParsedArchiveUrl, ParsedGitDirectoryUrl, ParsedGitPathUrl, ParsedUrl};
use uv_workspace::pyproject::Source;

use crate::commands::project::install_target::InstallTarget;

/// Extract any credentials that are defined on the workspace dependencies themselves. While we
/// don't store plaintext credentials in the `uv.lock`, we do respect credentials that are defined
/// in the `pyproject.toml`.
///
/// These credentials can come from any of `tool.uv.sources`, `tool.uv.dev-dependencies`,
/// `project.dependencies`, and `project.optional-dependencies`.
pub(crate) fn store_credentials_from_target(
    target: InstallTarget<'_>,
    client_builder: &BaseClientBuilder,
) -> Result<()> {
    // Iterate over any indexes in the target.
    for index in target.indexes() {
        if let Some(credentials) = index.credentials()? {
            if let Some(root_url) = index.root_url() {
                client_builder.store_credentials(&root_url, credentials.clone());
            }
            client_builder.store_credentials(index.raw_url(), credentials);
        }
    }

    // Iterate over any sources in the target.
    for source in target.sources() {
        match source {
            Source::Git { git, .. } => {
                uv_git::store_credentials_from_url(git)?;
            }
            Source::Url { url, .. } => {
                client_builder.store_credentials_from_url(url)?;
            }
            _ => {}
        }
    }

    // Iterate over any dependencies defined in the target.
    for requirement in target.requirements() {
        let Some(VersionOrUrl::Url(url)) = &requirement.version_or_url else {
            continue;
        };
        match &url.parsed_url {
            ParsedUrl::GitDirectory(ParsedGitDirectoryUrl { url, .. })
            | ParsedUrl::GitPath(ParsedGitPathUrl { url, .. }) => {
                uv_git::store_credentials_from_url(url.url())?;
            }
            ParsedUrl::Archive(ParsedArchiveUrl { url, .. }) => {
                client_builder.store_credentials_from_url(url)?;
            }
            _ => {}
        }
    }
    Ok(())
}
