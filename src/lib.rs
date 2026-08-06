//! Zed extension that launches `ebcdic-lsp`.
//!
//! Deliberately thin. Zed extensions are WebAssembly components with no access to editor
//! buffers, so all conversion logic lives in the `ebcdic-lsp` binary and reaches the editor as
//! LSP code actions. This crate's only jobs are finding that binary and forwarding settings.

use std::fs;

use zed_extension_api::{self as zed, settings::LspSettings, Result};

/// Releases of this repository carry the prebuilt server binaries.
const GITHUB_REPO: &str = "postalservice14/zed-ebcdicconverter";
const BINARY_STEM: &str = "ebcdic-lsp";

struct EbcdicConverterExtension {
    cached_binary_path: Option<String>,
}

fn binary_name() -> &'static str {
    if zed::current_platform().0 == zed::Os::Windows {
        "ebcdic-lsp.exe"
    } else {
        BINARY_STEM
    }
}

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn set_status(id: &zed::LanguageServerId, status: zed::LanguageServerInstallationStatus) {
    zed::set_language_server_installation_status(id, &status);
}

impl EbcdicConverterExtension {
    fn binary_path(
        &mut self,
        id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        // An explicit path in settings always wins, so a user can point at a local build.
        if let Some(path) = LspSettings::for_worktree(BINARY_STEM, worktree)
            .ok()
            .and_then(|settings| settings.binary)
            .and_then(|binary| binary.path)
        {
            return Ok(path);
        }

        // Then anything on PATH: this is what makes `cargo build && export PATH=...` work while
        // developing, without touching releases at all.
        if let Some(path) = worktree.which(binary_name()) {
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path {
            if is_file(path) {
                set_status(id, zed::LanguageServerInstallationStatus::None);
                return Ok(path.clone());
            }
        }

        let path = self.download(id)?;
        self.cached_binary_path = Some(path.clone());
        Ok(path)
    }

    /// Download the release asset for this platform, unless it is already unpacked.
    fn download(&self, id: &zed::LanguageServerId) -> Result<String> {
        set_status(id, zed::LanguageServerInstallationStatus::CheckingForUpdate);
        let release = zed::latest_github_release(
            GITHUB_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (platform, architecture) = zed::current_platform();
        let os = match platform {
            zed::Os::Mac => "darwin",
            zed::Os::Linux => "linux",
            zed::Os::Windows => "windows",
        };
        let arch = match architecture {
            zed::Architecture::Aarch64 => "arm64",
            zed::Architecture::X86 | zed::Architecture::X8664 => "amd64",
        };
        // Archives rather than bare binaries, so the executable bit survives the download.
        let (extension, file_type) = match platform {
            zed::Os::Windows => ("zip", zed::DownloadedFileType::Zip),
            _ => ("tar.gz", zed::DownloadedFileType::GzipTar),
        };
        let asset_name = format!("{BINARY_STEM}-{os}-{arch}.{extension}");

        let version_directory = format!("{BINARY_STEM}-{}", release.version);
        let binary_path = format!("{version_directory}/{}", binary_name());

        if !is_file(&binary_path) {
            set_status(id, zed::LanguageServerInstallationStatus::Downloading);
            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name == asset_name)
                .ok_or_else(|| {
                    format!(
                        "no {asset_name} in release {}. This platform may not have a prebuilt \
                         binary; build server/ with cargo and put {} on your PATH.",
                        release.version,
                        binary_name()
                    )
                })?;

            zed::download_file(&asset.download_url, &version_directory, file_type)
                .map_err(|error| format!("failed to download {asset_name}: {error}"))?;

            if !is_file(&binary_path) {
                return Err(format!("{asset_name} did not contain {}", binary_name()));
            }
            remove_other_versions(&version_directory);
        }

        set_status(id, zed::LanguageServerInstallationStatus::None);
        Ok(binary_path)
    }
}

/// Delete previously downloaded versions so the extension directory does not grow forever.
fn remove_other_versions(keep: &str) {
    let Ok(entries) = fs::read_dir(".") else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str() != Some(keep) {
            fs::remove_dir_all(entry.path()).ok();
        }
    }
}

impl zed::Extension for EbcdicConverterExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = self.binary_path(id, worktree).inspect_err(|error| {
            set_status(
                id,
                zed::LanguageServerInstallationStatus::Failed(error.to_string()),
            );
        })?;

        Ok(zed::Command {
            command,
            args: Vec::new(),
            env: Default::default(),
        })
    }

    /// Settings the server reads at startup, so a codepage filter applies to the first request.
    fn language_server_initialization_options(
        &mut self,
        _id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(BINARY_STEM, worktree)
            .ok()
            .and_then(|lsp| lsp.initialization_options.or(lsp.settings)))
    }

    /// The same settings delivered as configuration, so edits apply without a restart.
    fn language_server_workspace_configuration(
        &mut self,
        _id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(BINARY_STEM, worktree)
            .ok()
            .and_then(|lsp| lsp.settings))
    }
}

zed::register_extension!(EbcdicConverterExtension);
