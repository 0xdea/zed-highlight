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
    /// Constructs a new instance of the extension.
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    /// Returns the command used to start the language server for the specified language.
    ///
    /// # Errors
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
    /// Ensures the language server binary is available and returns its path. If the binary is not already cached and
    /// valid, checks GitHub for the latest release, downloads the appropriate prebuilt binary for the current platform,
    /// and caches its path for future use.
    ///
    /// # Errors
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

    /// Performs the actual install steps (fetch release metadata, download and extract the latest release archive,
    /// and make the binary executable) and returns the binary path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The latest release cannot be fetched from GitHub.
    /// - No suitable prebuilt binary asset is found for the current platform.
    /// - The binary fails to download, extract, or be made executable.
    ///
    /// Errors bubble up to [`HighlightExtension::ensure_binary`], which is responsible for reporting
    /// [`zed::LanguageServerInstallationStatus::Failed`] to the UI.
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
        let binary_path = binary_path_in_version(&release.version, os);

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
        }

        // Remove any other directories to avoid unbounded disk growth. Runs unconditionally so that we self-heal when a
        // previous install succeeded but the prune step failed or was interrupted on an earlier run.
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

/// Helper function to return the executable file name for the given OS.
///
/// On Windows the LSP binary is shipped as `zed-highlight-lsp.exe`; on Unix-like systems it has no extension. The
/// suffix matters because we use this name both to probe the cache with [`fs::metadata`] and to hand the path back to
/// Zed for [`zed::Command`] spawning.
fn binary_file_name(os: zed::Os) -> String {
    match os {
        zed::Os::Windows => format!("{BINARY_NAME}.exe"),
        zed::Os::Mac | zed::Os::Linux => BINARY_NAME.to_owned(),
    }
}

/// Helper function to return the path to the binary within its versioned directory for the given OS.
fn binary_path_in_version(version: &str, os: zed::Os) -> String {
    format!("{}/{}", version_dir_name(version), binary_file_name(os))
}

/// Registers as a Zed extension.
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

    // Test `version_dir_name`.

    #[test]
    fn version_dir_name_format_is_correct() {
        assert_eq!(version_dir_name("0.1.0"), "zed-highlight-lsp-0.1.0");
    }

    // Test `binary_file_name`.

    #[test]
    fn binary_file_name_on_unix_has_no_extension() {
        assert_eq!(binary_file_name(zed::Os::Mac), "zed-highlight-lsp");
        assert_eq!(binary_file_name(zed::Os::Linux), "zed-highlight-lsp");
    }

    #[test]
    fn binary_file_name_on_windows_has_exe_extension() {
        assert_eq!(binary_file_name(zed::Os::Windows), "zed-highlight-lsp.exe");
    }

    // Test `binary_path_in_version`.

    #[test]
    fn binary_path_in_version_format_is_correct_on_unix() {
        assert_eq!(
            binary_path_in_version("0.1.0", zed::Os::Mac),
            "zed-highlight-lsp-0.1.0/zed-highlight-lsp"
        );
        assert_eq!(
            binary_path_in_version("0.1.0", zed::Os::Linux),
            "zed-highlight-lsp-0.1.0/zed-highlight-lsp"
        );
    }

    #[test]
    fn binary_path_in_version_format_is_correct_on_windows() {
        // Regression test: on Windows the archive contains `zed-highlight-lsp.exe`, so the path used for cache probing
        // and for spawning the language server must include the `.exe` suffix. Forgetting it causes Zed to re-download
        // on every session and then fail to start the LSP because the resolved path doesn't exist.
        assert_eq!(
            binary_path_in_version("0.1.0", zed::Os::Windows),
            "zed-highlight-lsp-0.1.0/zed-highlight-lsp.exe"
        );
    }

    #[test]
    fn binary_path_uses_forward_slash() {
        // The path is passed to `zed::make_file_executable` and `zed::download_file`, both of which expect POSIX-style
        // paths because the extension runs inside a WASM sandbox.
        for os in [zed::Os::Mac, zed::Os::Linux, zed::Os::Windows] {
            let path = binary_path_in_version("0.1.0", os);
            assert!(
                path.contains('/'),
                "binary path must use '/' as separator (os: {os:?})"
            );
            assert!(
                !path.contains('\\'),
                "binary path must not use '\\' as separator (os: {os:?})"
            );
        }
    }
}
