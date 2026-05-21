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
        let asset_name = platform_asset_name(os, arch);

        // The versioned directory name encodes the release tag so that:
        // - A cached binary from the current version is reused without re-downloading.
        // - Upgrading to a new release stores the new binary in a different directory and cleans up the old ones.
        let version_dir = version_dir_name(&release.version);
        let binary_path = binary_path_in_version(&release.version);

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

/// Helper function to return the tarball asset name for the given OS and architecture.
///
/// Asset names follow the pattern `{BINARY_NAME}-{os}-{arch}.tar.gz` and are
/// matched against GitHub Release asset names during installation.
fn platform_asset_name(os: zed::Os, arch: zed::Architecture) -> String {
    let os = match os {
        zed::Os::Mac => "darwin",
        zed::Os::Linux => "linux",
        zed::Os::Windows => "windows",
    };
    let arch = match arch {
        zed::Architecture::Aarch64 => "aarch64",
        zed::Architecture::X8664 => "x86_64",
        zed::Architecture::X86 => "x86", // Not supported by Zed.
    };
    format!("{BINARY_NAME}-{os}-{arch}.tar.gz")
}

/// Helper function to return the versioned directory name used to cache a specific release on disk.
fn version_dir_name(version: &str) -> String {
    format!("{BINARY_NAME}-{version}")
}

/// Helper function to return the path to the binary within its versioned directory.
fn binary_path_in_version(version: &str) -> String {
    format!("{}/{BINARY_NAME}", version_dir_name(version))
}

/// Register as a Zed extension.
mod register {
    // The `register_extension!` macro expands to `pub` glue items that the WASM host imports by name.
    // Wrapping the call in a module prevents the `missing_docs` lint from firing.
    super::zed::register_extension!(super::HighlightExtension);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test constants.

    #[test]
    fn binary_name_is_correct() {
        assert_eq!(
            BINARY_NAME, "zed-highlight-lsp",
            "language server binary name must match the published crate"
        );
    }

    #[test]
    fn repository_slug_is_correct() {
        assert_eq!(
            REPOSITORY, "0xdea/zed-highlight",
            "repository must be the canonical GitHub slug"
        );
    }

    // Test `HighlightExtension::new`.

    #[test]
    fn new_starts_with_no_cached_path() {
        // Call `new` through the `zed::Extension` trait.
        let ext = <HighlightExtension as zed::Extension>::new();
        assert!(
            ext.cached_binary_path.is_none(),
            "a freshly created extension must not have a cached binary path"
        );
    }

    // Test `platform_asset_name`.

    #[test]
    fn asset_name_mac_aarch64_is_correct() {
        let name = platform_asset_name(zed::Os::Mac, zed::Architecture::Aarch64);
        assert_eq!(name, "zed-highlight-lsp-darwin-aarch64.tar.gz");
    }

    #[test]
    fn asset_name_mac_x86_64_is_correct() {
        let name = platform_asset_name(zed::Os::Mac, zed::Architecture::X8664);
        assert_eq!(name, "zed-highlight-lsp-darwin-x86_64.tar.gz");
    }

    #[test]
    fn asset_name_mac_x86_is_correct() {
        let name = platform_asset_name(zed::Os::Mac, zed::Architecture::X86);
        assert_eq!(name, "zed-highlight-lsp-darwin-x86.tar.gz");
    }

    #[test]
    fn asset_name_linux_aarch64_is_correct() {
        let name = platform_asset_name(zed::Os::Linux, zed::Architecture::Aarch64);
        assert_eq!(name, "zed-highlight-lsp-linux-aarch64.tar.gz");
    }

    #[test]
    fn asset_name_linux_x86_64_is_correct() {
        let name = platform_asset_name(zed::Os::Linux, zed::Architecture::X8664);
        assert_eq!(name, "zed-highlight-lsp-linux-x86_64.tar.gz");
    }

    #[test]
    fn asset_name_linux_x86_is_correct() {
        let name = platform_asset_name(zed::Os::Linux, zed::Architecture::X86);
        assert_eq!(name, "zed-highlight-lsp-linux-x86.tar.gz");
    }

    #[test]
    fn asset_name_windows_aarch64_is_correct() {
        let name = platform_asset_name(zed::Os::Windows, zed::Architecture::Aarch64);
        assert_eq!(name, "zed-highlight-lsp-windows-aarch64.tar.gz");
    }

    #[test]
    fn asset_name_windows_x86_64_is_correct() {
        let name = platform_asset_name(zed::Os::Windows, zed::Architecture::X8664);
        assert_eq!(name, "zed-highlight-lsp-windows-x86_64.tar.gz");
    }

    #[test]
    fn asset_name_windows_x86_is_correct() {
        let name = platform_asset_name(zed::Os::Windows, zed::Architecture::X86);
        assert_eq!(name, "zed-highlight-lsp-windows-x86.tar.gz");
    }

    #[test]
    fn asset_name_starts_with_binary_name() {
        for os in [zed::Os::Mac, zed::Os::Linux, zed::Os::Windows] {
            for arch in [
                zed::Architecture::Aarch64,
                zed::Architecture::X8664,
                zed::Architecture::X86,
            ] {
                let name = platform_asset_name(os, arch);
                assert!(
                    name.starts_with(BINARY_NAME),
                    "asset name '{name}' must start with `BINARY_NAME`"
                );
            }
        }
    }

    #[test]
    fn asset_name_ends_with_tar_gz() {
        for os in [zed::Os::Mac, zed::Os::Linux, zed::Os::Windows] {
            for arch in [
                zed::Architecture::Aarch64,
                zed::Architecture::X8664,
                zed::Architecture::X86,
            ] {
                let name = platform_asset_name(os, arch);
                assert!(
                    name.ends_with(".tar.gz"),
                    "asset name '{name}' must end with '.tar.gz'"
                );
            }
        }
    }

    // Test `version_dir_name`.

    #[test]
    fn version_dir_name_format_is_correct() {
        assert_eq!(version_dir_name("0.1.0"), "zed-highlight-lsp-0.1.0");
    }

    #[test]
    fn version_dir_name_starts_with_binary_name() {
        let dir = version_dir_name("1.2.3");
        assert!(
            dir.starts_with(BINARY_NAME),
            "version dir '{dir}' must start with `BINARY_NAME` so old-version cleanup is scoped correctly"
        );
    }

    #[test]
    fn version_dir_name_includes_version() {
        let version = "99.0.0-alpha";
        assert!(
            version_dir_name(version).contains(version),
            "version dir must contain the version string verbatim"
        );
    }

    // Test `binary_path_in_version`.

    #[test]
    fn binary_path_in_version_format_is_correct() {
        assert_eq!(
            binary_path_in_version("0.1.0"),
            "zed-highlight-lsp-0.1.0/zed-highlight-lsp"
        );
    }

    #[test]
    fn binary_path_in_version_is_under_version_dir() {
        let version = "0.1.0";
        let dir = version_dir_name(version);
        let path = binary_path_in_version(version);
        assert!(
            path.starts_with(&dir),
            "binary path '{path}' must be inside its version directory '{dir}'"
        );
    }

    #[test]
    fn binary_path_ends_with_binary_name() {
        let path = binary_path_in_version("0.1.0");
        assert!(
            path.ends_with(BINARY_NAME),
            "binary path '{path}' must end with `BINARY_NAME` so `make_file_executable` targets the right file"
        );
    }

    #[test]
    fn binary_path_uses_forward_slash() {
        // The path is passed to `zed::make_file_executable` and `zed::download_file`, both of which expect POSIX-style
        // paths because the extension runs inside a WASM sandbox.
        let path = binary_path_in_version("0.1.0");
        assert!(path.contains('/'), "binary path must use '/' as separator");
        assert!(
            !path.contains('\\'),
            "binary path must not use '\\' as separator"
        );
    }
}
