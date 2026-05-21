#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/logo.png"
)]

use std::fs;

use zed_extension_api::{self as zed, LanguageServerId, Result};

/// Repository slug used for GitHub release lookups and downloads.
const REPOSITORY: &str = "0xdea/zed-highlight";
/// Expected name of the language server binary.
const BINARY_NAME: &str = "zed-highlight-lsp";

/// Zed extension that allows to highlight all occurrences of selected words.
struct HighlightExtension {
    /// In-process cache of the resolved binary path. Avoids a redundant [`fs::metadata`] call on every
    /// [`zed::Extension::language_server_command`] invocation within the same Zed session. Not persisted across
    /// restarts; the versioned directory on disk serves that role.
    cached_binary_path: Option<String>,
}

impl zed::Extension for HighlightExtension {
    /// Construct a new instance of the extension.
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    /// Return the command used to start the language server for the specified language.
    ///
    /// ## Errors
    ///
    /// Returns an error if the language server binary cannot be found locally or downloaded.
    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        // Prefer a locally installed binary over downloading from GitHub, so dev builds work without a release.
        if let Some(path) = worktree.which(BINARY_NAME) {
            return Ok(zed::Command {
                command: path,
                args: vec![],
                env: vec![],
            });
        }

        // Fall back to the GitHub release mechanism for users who don't have a local build (e.g., non-developers).
        let binary_path = self.ensure_binary(language_server_id)?;
        Ok(zed::Command {
            command: binary_path,
            args: vec![],
            env: vec![],
        })
    }
}

impl HighlightExtension {
    /// Ensure the language server binary is available and return its path. If the binary is not already cached and
    /// valid, check GitHub for the latest release, download the appropriate prebuilt binary for the current platform,
    /// and cache its path for future use.
    ///
    /// ## Errors
    ///
    /// Returns an error if any step of the install process fails as reported by [`HighlightExtension::install_binary`].
    fn ensure_binary(&mut self, language_server_id: &LanguageServerId) -> Result<String> {
        // Immediately return the cached path if the file still exists on disk.
        if let Some(ref path) = self.cached_binary_path
            && fs::metadata(path).is_ok_and(|m| m.is_file())
        {
            return Ok(path.clone());
        }

        // Run the install pipeline. On error, surface a `Failed` status to Zed before propagating so the UI doesn't
        // get stuck on `CheckingForUpdate` or `Downloading`.
        let result = Self::install_binary(language_server_id);

        match result {
            Ok(binary_path) => {
                // Reset Zed language server installation status indicator and populate the cache.
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::None,
                );
                self.cached_binary_path = Some(binary_path.clone());
                Ok(binary_path)
            }
            Err(e) => {
                // Report failure to Zed so the UI can update accordingly.
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::Failed(e.clone()),
                );
                Err(e)
            }
        }
    }

    /// Perform the actual install steps (fetch release metadata, download and extract the latest release archive,
    /// and make the binary executable). Errors bubble up to [`HighlightExtension::ensure_binary`], which is
    /// responsible for reporting [`zed::LanguageServerInstallationStatus::Failed`] to the UI.
    ///
    /// ## Errors
    ///
    /// Returns an error if:
    /// - The latest release cannot be fetched from GitHub.
    /// - No suitable prebuilt binary asset is found for the current platform.
    /// - The binary fails to download, extract, or be made executable.
    fn install_binary(language_server_id: &LanguageServerId) -> Result<String> {
        // Tell Zed we are checking for an update.
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        // Fetch the latest release from GitHub. Skip pre-releases and tag-only releases with no attached binaries.
        let release = zed::latest_github_release(
            REPOSITORY,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )
        .map_err(|e| format!("failed to fetch latest GitHub release: {e}"))?;

        // Determine which prebuilt asset to download for the current platform.
        let (os, arch) = zed::current_platform();
        let asset_name = format!(
            "{BINARY_NAME}-{os}-{arch}.tar.gz",
            os = match os {
                zed::Os::Mac => "darwin",
                zed::Os::Linux => "linux",
                zed::Os::Windows => "windows",
            },
            arch = match arch {
                zed::Architecture::Aarch64 => "aarch64",
                zed::Architecture::X8664 => "x86_64",
                zed::Architecture::X86 => "x86", // Not supported by Zed.
            },
        );

        // The versioned directory name encodes the release tag so that:
        // - A cached binary from the current version is reused without re-downloading.
        // - Upgrading to a new release stores the new binary in a different directory and cleans up the old ones.
        let version_dir = format!("{BINARY_NAME}-{}", release.version);
        let binary_path = format!("{version_dir}/{BINARY_NAME}");

        // Download and extract the binary if it's not already present.
        if !fs::metadata(&binary_path).is_ok_and(|m| m.is_file()) {
            // Tell Zed we are downloading the update.
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );

            let asset = release
                .assets
                .iter()
                .find(|a| a.name == asset_name)
                .ok_or_else(|| {
                    format!(
                        "no prebuilt binary found for {asset_name}. \
                         Build {BINARY_NAME} manually and place it on your PATH."
                    )
                })?;

            // Extract the archive into `version_dir` (relative to the extension's Zed-managed working directory).
            zed::download_file(
                &asset.download_url,
                &version_dir,
                zed::DownloadedFileType::GzipTar,
            )
            .map_err(|e| format!("failed to download {BINARY_NAME}: {e}"))?;

            // Ensure the binary is executable (this is a no-op on Windows or if the bit is already set).
            zed::make_file_executable(&binary_path)
                .map_err(|e| format!("failed to make {BINARY_NAME} executable: {e}"))?;

            // Remove all other directories to avoid unbounded disk growth.
            #[expect(
                clippy::let_underscore_must_use,
                reason = "Errors here are non-fatal and can be safely ignored."
            )]
            if let Ok(entries) = fs::read_dir(".") {
                for entry in entries.flatten() {
                    if entry.file_name().to_str() != Some(&version_dir) {
                        let _ = fs::remove_dir_all(entry.path());
                    }
                }
            }
        }

        Ok(binary_path)
    }
}

/// Register as a Zed extension.
mod register {
    // The `register_extension!` macro expands to `pub` glue items that the WASM host imports by name.
    // Wrapping the call in a module prevents the `missing_docs` lint from firing.
    super::zed::register_extension!(super::HighlightExtension);
}

// TODO: add tests.
/*
#[cfg(test)]
mod tests {
    use super::*;

    // Test constants.
    const EXPECTED_SUM: i32 = 4;
    const EXPECTED_RESULT: &str = "Expected result string";

    #[test]
    fn it_works() {
        // Arrange.
        // Act.
        // Assert.
        assert_eq!(2 + 2, EXPECTED_SUM, "It should work!");
    }
}
*/
