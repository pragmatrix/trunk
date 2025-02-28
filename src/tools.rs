//! Download management for external tools and applications. Locate and automatically download
//! applications (if needed) to use them in the build pipeline.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, ensure, Context, Result};
use directories::ProjectDirs;
use futures_util::stream::StreamExt;
use once_cell::sync::Lazy;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell};

use self::archive::Archive;
use crate::common::is_executable;

/// The application to locate and eventually download when calling [`get`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Application {
    Sass,
    /// wasm-bindgen for generating the JS bindings.
    WasmBindgen,
    /// wasm-opt to improve performance and size of the output file further.
    WasmOpt,
}

impl Application {
    /// Base name of the executable without extension.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Sass => "sass",
            Self::WasmBindgen => "wasm-bindgen",
            Self::WasmOpt => "wasm-opt",
        }
    }

    /// Path of the executable within the downloaded archive.
    fn path(&self) -> &str {
        if cfg!(target_os = "windows") {
            match self {
                Self::Sass => "sass.bat",
                Self::WasmBindgen => "wasm-bindgen.exe",
                Self::WasmOpt => "bin/wasm-opt.exe",
            }
        } else {
            match self {
                Self::Sass => "sass",
                Self::WasmBindgen => "wasm-bindgen",
                Self::WasmOpt => "bin/wasm-opt",
            }
        }
    }

    /// Additional files included in the archive that are required to run the main binary.
    fn extra_paths(&self) -> &[&str] {
        match self {
            Self::Sass => {
                if cfg!(target_os = "windows") {
                    &["src/dart.exe", "src/sass.snapshot"]
                } else if cfg!(target_os = "macos") {
                    &["src/dart", "src/sass.snapshot"]
                } else {
                    &[]
                }
            }
            Self::WasmBindgen => &[],
            Self::WasmOpt => {
                if cfg!(target_os = "macos") {
                    &["lib/libbinaryen.dylib"]
                } else {
                    &[]
                }
            }
        }
    }

    /// Default version to use if not set by the user.
    fn default_version(&self) -> &str {
        match self {
            Self::Sass => "1.54.9",
            Self::WasmBindgen => "0.2.83",
            Self::WasmOpt => "version_110",
        }
    }

    /// Direct URL to the release of an application for download.
    fn url(&self, version: &str) -> Result<String> {
        let target_os = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            bail!("unsupported OS")
        };

        let target_arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            bail!("unsupported target architecture")
        };

        Ok(match self {
            Self::Sass => match (target_os, target_arch) {
              ("windows", "x86_64") => format!("https://github.com/sass/dart-sass/releases/download/{version}/dart-sass-{version}-windows-x64.zip"),
              ("macos" | "linux", "x86_64") => format!("https://github.com/sass/dart-sass/releases/download/{version}/dart-sass-{version}-{target_os}-x64.tar.gz"),
              ("macos" | "linux", "aarch64") => format!("https://github.com/sass/dart-sass/releases/download/{version}/dart-sass-{version}-{target_os}-arm64.tar.gz"),
              _ => bail!("Unable to download Sass for {target_os} {target_arch}")
            },

            Self::WasmBindgen => format!(
                "https://github.com/rustwasm/wasm-bindgen/releases/download/{version}/wasm-bindgen-{version}-x86_64-{os}.tar.gz",
                os = match target_os {
                "windows" => "pc-windows-msvc",
                "macos" => "apple-darwin",
                "linux" => "unknown-linux-musl",
                _ => unreachable!(),
              }),

            Self::WasmOpt => match (target_os, target_arch) {
              ("macos", "aarch64") => format!("https://github.com/WebAssembly/binaryen/releases/download/{version}/binaryen-{version}-arm64-macos.tar.gz"),
              _ => format!("https://github.com/WebAssembly/binaryen/releases/download/{version}/binaryen-{version}-{target_arch}-{target_os}.tar.gz")
            }
        })
    }

    /// The CLI subcommand, flag or option used to check the application's version.
    fn version_test(&self) -> &'static str {
        match self {
            Application::Sass => "--version",
            Application::WasmBindgen => "--version",
            Application::WasmOpt => "--version",
        }
    }

    /// Format the output of version checking the app.
    fn format_version_output(&self, text: &str) -> Result<String> {
        let text = text.trim();
        let formatted_version = match self {
            Application::Sass => text
                .lines()
                .next()
                .with_context(|| format!("missing or malformed version output: {}", text))?
                .to_owned(),
            Application::WasmBindgen => text
                .split(' ')
                .nth(1)
                .with_context(|| format!("missing or malformed version output: {}", text))?
                .to_owned(),
            Application::WasmOpt => format!(
                "version_{}",
                text.split(' ')
                    .nth(2)
                    .with_context(|| format!("missing or malformed version output: {}", text))?
            ),
        };
        Ok(formatted_version)
    }
}

/// Global, application wide app cache that keeps track of what tools have already been
/// downloaded and installed to avoid duplicate installation runs.
static GLOBAL_APP_CACHE: Lazy<Mutex<AppCache>> = Lazy::new(|| Mutex::new(AppCache::new()));

/// An app cache that does the actual download and installation of tools while keeping track of
/// what has already been installed in the current trunk execution.
///
/// This cache doesn't keep track of any system-installed tools or the one's that have been
/// installed in previous runs of trunk. It only helps in avoiding a download of the same tool
/// concurrently during a single run of trunk.
struct AppCache(HashMap<(Application, String), OnceCell<()>>);

impl AppCache {
    /// Create a new app cache.
    fn new() -> Self {
        Self(HashMap::new())
    }

    /// Install the desired application of given version to the provided application directory. Or
    /// don't if it's already been installed.
    async fn install_once(
        &mut self,
        app: Application,
        version: &str,
        app_dir: PathBuf,
    ) -> Result<()> {
        let cached = self
            .0
            .entry((app, version.to_owned()))
            .or_insert_with(OnceCell::new);

        cached
            .get_or_try_init(|| async move {
                let path = download(app, version)
                    .await
                    .context("failed downloading release archive")?;

                let file = File::open(&path)
                    .await
                    .context("failed opening downloaded file")?;
                install(app, file, app_dir).await?;
                tokio::fs::remove_file(path)
                    .await
                    .context("failed deleting temporary archive")?;

                Ok(())
            })
            .await
            .map(|_| ())
    }
}

/// Locate the given application and download it if missing.
#[tracing::instrument(level = "trace")]
pub async fn get(app: Application, version: Option<&str>) -> Result<PathBuf> {
    if let Some((path, version)) = find_system(app, version).await {
        tracing::info!(app = %app.name(), %version, "using system installed binary");
        return Ok(path);
    }

    let cache_dir = cache_dir().await?;
    let version = version.unwrap_or_else(|| app.default_version());
    let app_dir = cache_dir.join(format!("{}-{}", app.name(), version));
    let bin_path = app_dir.join(app.path());

    if !is_executable(&bin_path).await? {
        GLOBAL_APP_CACHE
            .lock()
            .await
            .install_once(app, version, app_dir)
            .await?;
    }

    Ok(bin_path)
}

/// Try to find a globally system installed version of the application and ensure it is the needed
/// release version.
#[tracing::instrument(level = "trace")]
async fn find_system(app: Application, version: Option<&str>) -> Option<(PathBuf, String)> {
    let result = || async {
        let path = which::which(app.name())?;
        let output = Command::new(&path).arg(app.version_test()).output().await?;
        ensure!(
            output.status.success(),
            "running command `{} {}` failed",
            path.display(),
            app.version_test()
        );

        let text = String::from_utf8_lossy(&output.stdout);
        let system_version = app.format_version_output(&text)?;

        Ok((path, system_version))
    };

    match result().await {
        Ok((path, system_version)) => version
            .map(|v| v == system_version)
            .unwrap_or(true)
            .then(|| (path, system_version)),
        Err(e) => {
            tracing::debug!("system version not found for {}: {}", app.name(), e);
            None
        }
    }
}

/// Download a file from its remote location in the given version, extract it and make it ready for
/// execution at the given location.
#[tracing::instrument(level = "trace")]
async fn download(app: Application, version: &str) -> Result<PathBuf> {
    tracing::info!(version = version, "downloading {}", app.name());

    let cache_dir = cache_dir()
        .await
        .context("failed getting the cache directory")?;
    let temp_out = cache_dir.join(format!("{}-{}.tmp", app.name(), version));
    let mut file = File::create(&temp_out)
        .await
        .context("failed creating temporary output file")?;

    let resp = reqwest::get(app.url(version)?)
        .await
        .context("error sending HTTP request")?;
    ensure!(
        resp.status().is_success(),
        "error downloading archive file: {:?}\n{}",
        resp.status(),
        app.url(version)?
    );
    let mut res_bytes = resp.bytes_stream();
    while let Some(chunk_res) = res_bytes.next().await {
        let chunk = chunk_res.context("error reading chunk from download")?;
        let _res = file.write(chunk.as_ref()).await;
    }

    Ok(temp_out)
}

/// Install an application from a downloaded archive locating and copying it to the given target
/// location.
#[tracing::instrument(level = "trace")]
async fn install(app: Application, archive_file: File, target: PathBuf) -> Result<()> {
    tracing::info!("installing {}", app.name());

    let archive_file = archive_file.into_std().await;

    tokio::task::spawn_blocking(move || {
        let mut archive = if app == Application::Sass && cfg!(target_os = "windows") {
            Archive::new_zip(archive_file)?
        } else {
            Archive::new_tar_gz(archive_file)
        };
        archive.extract_file(app.path(), &target)?;

        for path in app.extra_paths() {
            // After extracting one file the archive must be reset.
            archive = archive.reset()?;
            archive.extract_file(path, &target)?;
        }

        Ok(())
    })
    .await?
}

/// Locate the cache dir for trunk and make sure it exists.
pub async fn cache_dir() -> Result<PathBuf> {
    let path = ProjectDirs::from("dev", "trunkrs", "trunk")
        .context("failed finding project directory")?
        .cache_dir()
        .to_owned();
    tokio::fs::create_dir_all(&path)
        .await
        .context("failed creating cache directory")?;
    Ok(path)
}

mod archive {
    use std::fs::{self, File};
    use std::io::{self, BufReader, Read, Seek, SeekFrom};
    use std::path::Path;

    use anyhow::{Context, Result};
    use flate2::read::GzDecoder;
    use tar::{Archive as TarArchive, Entry as TarEntry};
    use zip::ZipArchive;

    pub enum Archive {
        TarGz(Box<TarArchive<GzDecoder<BufReader<File>>>>),
        Zip(ZipArchive<BufReader<File>>),
    }

    impl Archive {
        pub fn new_tar_gz(file: File) -> Self {
            Self::TarGz(Box::new(TarArchive::new(GzDecoder::new(BufReader::new(
                file,
            )))))
        }

        pub fn new_zip(file: File) -> Result<Self> {
            Ok(Self::Zip(ZipArchive::new(BufReader::new(file))?))
        }

        pub fn extract_file(&mut self, file: &str, target: &Path) -> Result<()> {
            match self {
                Self::TarGz(archive) => {
                    let mut tar_file =
                        find_tar_entry(archive, file)?.context("file not found in archive")?;
                    let mut out_file = extract_file(&mut tar_file, file, target)?;

                    if let Ok(mode) = tar_file.header().mode() {
                        set_file_permissions(&mut out_file, mode)?;
                    }
                }
                Self::Zip(archive) => {
                    let zip_index =
                        find_zip_entry(archive, file)?.context("file not found in archive")?;
                    let mut zip_file = archive.by_index(zip_index)?;
                    let mut out_file = extract_file(&mut zip_file, file, target)?;

                    if let Some(mode) = zip_file.unix_mode() {
                        set_file_permissions(&mut out_file, mode)?;
                    }
                }
            }

            Ok(())
        }

        pub fn reset(self) -> Result<Self> {
            match self {
                Self::TarGz(archive) => {
                    let mut archive_file = archive.into_inner().into_inner();
                    archive_file
                        .seek(SeekFrom::Start(0))
                        .context("error seeking to beginning of archive")?;

                    Ok(Self::TarGz(Box::new(TarArchive::new(GzDecoder::new(
                        archive_file,
                    )))))
                }
                Self::Zip(archive) => Ok(Self::Zip(archive)),
            }
        }
    }

    /// Find an entry in a TAR archive by name and open it for reading. The first part of the path
    /// is dropped as that's usually the folder name it was created from.
    fn find_tar_entry(
        archive: &mut TarArchive<impl Read>,
        path: impl AsRef<Path>,
    ) -> Result<Option<TarEntry<impl Read>>> {
        let entries = archive
            .entries()
            .context("failed getting archive entries")?;
        for entry in entries {
            let entry = entry.context("error while getting archive entry")?;
            let name = entry.path().context("invalid entry path")?;

            let mut name = name.components();
            name.next();

            if name.as_path() == path.as_ref() {
                return Ok(Some(entry));
            }
        }

        Ok(None)
    }

    /// Find an entry in a ZIP archive by name and return its index. The first part of the path is
    /// dropped as that's usually the folder name it was created from.
    fn find_zip_entry(
        archive: &mut ZipArchive<impl Read + Seek>,
        path: impl AsRef<Path>,
    ) -> Result<Option<usize>> {
        for index in 0..archive.len() {
            let entry = archive
                .by_index(index)
                .context("error while getting archive entry")?;
            let name = entry.enclosed_name().context("invalid entry path")?;

            let mut name = name.components();
            name.next();

            if name.as_path() == path.as_ref() {
                return Ok(Some(index));
            }
        }

        Ok(None)
    }

    fn extract_file(mut read: impl Read, file: &str, target: &Path) -> Result<File> {
        let out = target.join(file);

        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).context("failed creating output directory")?;
        }

        let mut out = File::create(target.join(file)).context("failed creating output file")?;
        io::copy(&mut read, &mut out)
            .context("failed copying over final output file from archive")?;

        Ok(out)
    }

    /// Set the executable flag for a file. Only has an effect on UNIX platforms.
    fn set_file_permissions(file: &mut File, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;

            file.set_permissions(Permissions::from_mode(mode))
                .context("failed setting file permissions")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{ensure, Context, Result};

    use super::*;

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn download_and_install_binaries() -> Result<()> {
        let dir = tempfile::tempdir().context("error creating temporary dir")?;

        for &app in &[
            Application::Sass,
            Application::WasmBindgen,
            Application::WasmOpt,
        ] {
            let path = download(app, app.default_version())
                .await
                .context("error downloading app")?;
            let file = File::open(&path).await.context("error opening file")?;
            install(app, file, dir.path().to_owned())
                .await
                .context("error installing app")?;
            std::fs::remove_file(path).context("error during cleanup")?;
        }
        Ok(())
    }

    macro_rules! table_test_format_version {
        ($name:ident, $app:expr, $input:literal, $expect:literal) => {
            #[test]
            fn $name() -> Result<()> {
                let app = $app;
                let output = app
                    .format_version_output($input)
                    .context("unexpected version formatting error")?;
                ensure!(
                    output == $expect,
                    "version check output does not match: {} != {}",
                    $expect,
                    output
                );
                Ok(())
            }
        };
    }

    table_test_format_version!(
        wasm_opt_from_source,
        Application::WasmOpt,
        "wasm-opt version 101 (version_101)",
        "version_101"
    );

    table_test_format_version!(
        wasm_opt_pre_compiled,
        Application::WasmOpt,
        "wasm-opt version 101",
        "version_101"
    );

    table_test_format_version!(
        wasm_bindgen_from_source,
        Application::WasmBindgen,
        "wasm-bindgen 0.2.75",
        "0.2.75"
    );

    table_test_format_version!(
        wasm_bindgen_pre_compiled,
        Application::WasmBindgen,
        "wasm-bindgen 0.2.74 (27c7a4d06)",
        "0.2.74"
    );

    table_test_format_version!(sass_pre_compiled, Application::Sass, "1.37.5", "1.37.5");
}
