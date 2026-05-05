#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/logo.png"
)]

use std::fs;

use zed_extension_api::{self as zed, LanguageServerId, Result};

/// Zed extension that allows to highlight all occurrences of selected words.
struct ZedHighlightExtension {
    /// In-process cache of the resolved binary path. Avoids a redundant `fs::metadata` call on every
    /// `language_server_command` invocation within the same Zed session. Not persisted across restarts;
    /// the versioned directory on disk serves that role.
    cached_binary_path: Option<String>,
}

impl zed::Extension for ZedHighlightExtension {
    /// Construct and return a new instance of the extension.
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    /// Return the command used to start the language server for the specified language.
    ///
    /// ## Errors
    ///
    /// Returns an error if the language server binary cannot be found or downloaded.
    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        // Local-first lookup. Prefer a locally installed binary (e.g., `cargo install --path lsp`)
        // over downloading from GitHub, so dev builds work without a release.
        if let Some(path) = worktree.which("zed-highlight-lsp") {
            return Ok(zed::Command {
                command: path,
                args: vec![],
                env: vec![],
            });
        }

        // Fall back to the GitHub release mechanism for users who don't have a local build.
        // This also serves as the default installation method for non-developers.
        let binary_path = self.ensure_binary(language_server_id)?;
        Ok(zed::Command {
            command: binary_path,
            args: vec![],
            env: vec![],
        })
    }
}

impl ZedHighlightExtension {
    /// Ensure the language server binary is available and return its path. If the binary is not already cached and
    /// valid, check GitHub for the latest release, download the appropriate prebuilt binary for the current platform,
    /// and cache its path for future use.
    ///
    /// ## Errors
    ///
    /// Returns an error if:
    /// - The latest release cannot be fetched from GitHub.
    /// - No suitable prebuilt binary asset is found for the current platform.
    /// - The binary fails to download or extract.
    fn ensure_binary(&mut self, language_server_id: &LanguageServerId) -> Result<String> {
        // Fast path: return the cached path if the file still exists on disk.
        // The file could have been deleted by the user or an OS cleanup tool,
        // so we verify with `fs::metadata` rather than trusting the cache blindly.
        if let Some(ref path) = self.cached_binary_path
            && fs::metadata(path).is_ok_and(|m| m.is_file())
        {
            return Ok(path.clone());
        }

        // Tell Zed we are checking for an update (shown in the status bar).
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        // Fetch the latest release from GitHub.
        let release = zed::latest_github_release(
            "0xdea/zed-highlight",
            zed::GithubReleaseOptions {
                require_assets: true, // Skip tag-only releases with no attached binaries
                pre_release: false,   // Skip pre-releases
            },
        )?;

        // Determine which prebuilt asset to download for the current platform.
        let (os, arch) = zed::current_platform();
        let asset_name = format!(
            "zed-highlight-lsp-{os}-{arch}.tar.gz",
            os = match os {
                zed::Os::Mac => "darwin",
                zed::Os::Linux => "linux",
                zed::Os::Windows => "windows",
            },
            arch = match arch {
                zed::Architecture::Aarch64 => "aarch64",
                zed::Architecture::X86 => "x86",
                zed::Architecture::X8664 => "x86_64",
            },
        );

        // The versioned directory name encodes the release tag so that:
        // - A cached binary from the current version is reused without re-downloading.
        // - Upgrading to a new release stores the new binary in a different directory and cleans up the old one.
        let version_dir = format!("zed-highlight-lsp-{}", release.version);
        let binary_path = format!("{version_dir}/zed-highlight-lsp");

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
                         Build zed-highlight-lsp manually and place it on your PATH."
                    )
                })?;

            // Extract the archive into `version_dir` (a path relative to the extension's Zed-managed
            // working directory). After extraction, the binary lives at `version_dir/zed-highlight-lsp`.
            zed::download_file(
                &asset.download_url,
                &version_dir,
                zed::DownloadedFileType::GzipTar,
            )
            .map_err(|e| format!("failed to download zed-highlight-lsp: {e}"))?;

            // Ensure the binary is executable (tar.gz preserves permissions but calling this is a no-op if the bit is
            // already set or on Windows, so it's safe to always call).
            zed::make_file_executable(&binary_path)?;

            // Remove all other versioned directories to avoid unbounded disk growth as releases accumulate.
            // Errors here an non-fatal.
            if let Ok(entries) = fs::read_dir(".") {
                for entry in entries.flatten() {
                    if entry.file_name().to_str() != Some(&version_dir) {
                        fs::remove_dir_all(entry.path()).ok();
                    }
                }
            }
        }

        // Reset Zed's language server installation status indicator.
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::None,
        );

        // Populate the cache and return the binary path.
        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

// Register as a Zed extension
zed::register_extension!(ZedHighlightExtension);

/*
#[cfg(test)]
mod tests {
    use super::*;

    // Test constants
    const EXPECTED_SUM: i32 = 4;
    const EXPECTED_RESULT: &str = "Expected result string";

    #[test]
    fn it_works() {
        // Arrange
        // Act
        // Assert
        assert_eq!(2 + 2, EXPECTED_SUM, "It should work!");
    }
}
*/
