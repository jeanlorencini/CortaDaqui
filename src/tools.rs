use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, tempdir_in};

use crate::worker::Job;

#[derive(Clone, Copy, Debug)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp",
            Self::Ffmpeg => "FFmpeg",
            Self::Deno => "Deno",
        }
    }

    fn repo(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp/yt-dlp",
            Self::Ffmpeg => "GyanD/codexffmpeg",
            Self::Deno => "denoland/deno",
        }
    }

    fn asset_name(self, version: &str) -> String {
        match self {
            Self::YtDlp => "yt-dlp.exe".to_owned(),
            Self::Ffmpeg => format!("ffmpeg-{version}-essentials_build.zip"),
            Self::Deno => "deno-x86_64-pc-windows-msvc.zip".to_owned(),
        }
    }

    fn binaries(self) -> &'static [&'static str] {
        match self {
            Self::YtDlp => &["yt-dlp.exe"],
            Self::Ffmpeg => &["ffmpeg.exe", "ffprobe.exe"],
            Self::Deno => &["deno.exe"],
        }
    }
}

#[derive(Clone)]
pub struct Toolkit {
    root: PathBuf,
    http: ureq::Agent,
}

#[derive(Clone)]
pub struct Binaries {
    pub yt_dlp: PathBuf,
    pub ffmpeg_dir: PathBuf,
    pub deno: PathBuf,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Clone, Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Installed {
    version: String,
    sha256: String,
}

impl Installed {
    fn directory(&self, root: &Path) -> Option<PathBuf> {
        (valid_version(&self.version) && valid_hash(&self.sha256))
            .then(|| root.join(format!("{}-{}", self.version, &self.sha256[..12])))
    }
}

#[derive(Clone)]
pub struct PendingRelease {
    tool: Tool,
    state: Installed,
    asset: Asset,
}

impl PendingRelease {
    pub fn version(&self) -> &str {
        &self.state.version
    }
}

impl Toolkit {
    pub fn new(root: PathBuf) -> Self {
        let http = ureq::Agent::config_builder()
            .https_only(true)
            .user_agent(concat!("CortaDaqui/", env!("CARGO_PKG_VERSION")))
            .timeout_resolve(Some(Duration::from_secs(12)))
            .timeout_connect(Some(Duration::from_secs(12)))
            .timeout_recv_response(Some(Duration::from_secs(20)))
            .timeout_global(Some(Duration::from_secs(15 * 60)))
            .build()
            .into();
        Self { root, http }
    }

    pub fn http(&self) -> &ureq::Agent {
        &self.http
    }

    fn installed(&self, tool: Tool) -> Option<(Installed, PathBuf)> {
        let root = self.root.join(tool.name());
        let file = File::open(root.join("current.json")).ok()?;
        let state: Installed = serde_json::from_reader(file.take(4096)).ok()?;
        let dir = state.directory(&root)?;
        tool.binaries()
            .iter()
            .all(|file| dir.join(file).is_file())
            .then_some((state, dir))
    }

    pub fn version(&self, tool: Tool) -> Option<String> {
        self.installed(tool).map(|(state, _)| state.version)
    }

    pub fn has_release(&self, release: &PendingRelease) -> bool {
        self.installed(release.tool).is_some_and(|(state, _)| {
            state.version == release.state.version && state.sha256 == release.state.sha256
        })
    }

    pub fn binaries(&self) -> Option<Binaries> {
        let (_, yt_dlp) = self.installed(Tool::YtDlp)?;
        let (_, ffmpeg_dir) = self.installed(Tool::Ffmpeg)?;
        let (_, deno) = self.installed(Tool::Deno)?;
        Some(Binaries {
            yt_dlp: yt_dlp.join("yt-dlp.exe"),
            ffmpeg_dir,
            deno: deno.join("deno.exe"),
        })
    }

    pub fn latest(&self, tool: Tool) -> Result<PendingRelease> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            tool.repo()
        );
        let release: Release = self
            .http
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .call()
            .with_context(|| {
                format!(
                    "Não foi possível consultar {}. Verifique a conexão.",
                    tool.name()
                )
            })?
            .body_mut()
            .with_config()
            .limit(2 * 1024 * 1024)
            .read_json()
            .context("O GitHub retornou um catálogo de versões inválido.")?;
        let version = release.tag_name;
        ensure!(
            valid_version(&version),
            "O fornecedor publicou uma versão inválida."
        );
        let asset = release
            .assets
            .into_iter()
            .find(|asset| asset.name == tool.asset_name(&version))
            .with_context(|| {
                format!(
                    "A versão {version} de {} não tem um binário para Windows x64.",
                    tool.name()
                )
            })?;
        let sha256 = asset
            .digest
            .as_deref()
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .filter(|hash| valid_hash(hash))
            .context("O fornecedor não disponibilizou o SHA-256 do binário.")?
            .to_ascii_lowercase();
        Ok(PendingRelease {
            tool,
            state: Installed { version, sha256 },
            asset,
        })
    }

    pub fn prepare(&self, tool: Tool, job: &Job) -> Result<()> {
        if matches!(tool, Tool::YtDlp) && self.installed(Tool::Deno).is_none() {
            self.prepare(Tool::Deno, job)?;
        }
        if self.installed(tool).is_none() {
            job.check()?;
            let label = format!("Consultando {}…", tool.name());
            job.progress(&label, None);
            self.install(&self.latest(tool)?, job)?;
        }
        Ok(())
    }

    pub fn install(&self, release: &PendingRelease, job: &Job) -> Result<()> {
        let tool = release.tool;
        let root = self.root.join(tool.name());
        fs::create_dir_all(&root).context("Não foi possível criar a pasta de ferramentas.")?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("install.lock"))?;
        lock.try_lock()
            .context("Outra janela está instalando esta ferramenta. Tente novamente.")?;
        job.check()?;
        if self.has_release(release) {
            return Ok(());
        }

        let destination = release.state.directory(&root).context("Versão inválida.")?;
        let stage = tempdir_in(&root)?;
        let asset = stage.path().join(&release.asset.name);
        self.download(release, &asset, job)?;
        job.progress(&format!("Instalando {}…", tool.name()), None);
        let files = stage.path().join("bin");
        fs::create_dir(&files)?;
        if matches!(tool, Tool::Ffmpeg | Tool::Deno) {
            extract_zip(tool, &asset, &files, job)?;
        } else {
            fs::rename(&asset, files.join(tool.binaries()[0]))?;
        }
        job.check()?;
        // A pasta inclui o hash. Uma versão já publicada pode estar em uso por outra janela.
        if !destination.exists() {
            fs::rename(&files, &destination)?;
        } else {
            ensure!(
                tool.binaries()
                    .iter()
                    .all(|name| destination.join(name).is_file()),
                "A pasta desta versão está incompleta. Mova-a e tente novamente: {}",
                destination.display()
            );
        }
        let mut active = NamedTempFile::new_in(&root)?;
        serde_json::to_writer(active.as_file_mut(), &release.state)?;
        active.as_file().sync_all()?;
        active
            .persist(root.join("current.json"))
            .context("Não foi possível ativar a nova versão.")?;
        Ok(())
    }

    fn download(&self, release: &PendingRelease, target: &Path, job: &Job) -> Result<()> {
        let size = release.asset.size;
        ensure!(
            (1..=512 * 1024 * 1024).contains(&size),
            "O tamanho do download é inválido."
        );
        let url = format!(
            "https://github.com/{}/releases/download/{}/{}",
            release.tool.repo(),
            release.version(),
            release.asset.name
        );
        let label = format!("Baixando {}…", release.tool.name());
        job.progress(&label, Some(0.0));
        let mut response = self.http.get(&url).call().with_context(|| {
            format!(
                "Não foi possível baixar {}. Verifique a conexão.",
                release.tool.name()
            )
        })?;
        let mut reader = response.body_mut().with_config().limit(size + 1).reader();
        let mut output = File::create(target)?;
        let mut hash = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            job.check()?;
            let count = reader
                .read(&mut buffer)
                .context("O download foi interrompido. Tente novamente.")?;
            if count == 0 {
                break;
            }
            total += count as u64;
            ensure!(
                total <= size,
                "O download tem tamanho diferente do publicado."
            );
            output.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
            job.progress(&label, Some(total as f32 / size as f32));
        }
        ensure!(
            total == size,
            "O download está incompleto. Tente novamente."
        );
        ensure!(
            format!("{:x}", hash.finalize()) == release.state.sha256,
            "O SHA-256 do download não confere."
        );
        Ok(())
    }
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 80
        && version.as_bytes()[0].is_ascii_alphanumeric()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn extract_zip(tool: Tool, asset: &Path, files: &Path, job: &Job) -> Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(asset)?)
        .with_context(|| format!("O ZIP de {} não abriu.", tool.name()))?;
    for index in 0..zip.len() {
        job.check()?;
        let mut entry = zip.by_index(index)?;
        let mut parts = entry.name().rsplit('/');
        let Some(name) = parts.next() else { continue };
        let parent = parts.next();
        let expected_location = match tool {
            Tool::Ffmpeg => parent == Some("bin"),
            Tool::Deno => parent.is_none(),
            Tool::YtDlp => false,
        };
        if !expected_location || !tool.binaries().contains(&name) || !entry.is_file() {
            continue;
        }
        ensure!(
            entry.size() <= 256 * 1024 * 1024,
            "O executável de {} é grande demais.",
            tool.name()
        );
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(files.join(name))?;
        io::copy(&mut entry, &mut output)?;
    }
    ensure!(
        tool.binaries()
            .iter()
            .all(|file| files.join(file).is_file()),
        "O ZIP não contém os executáveis de {}.",
        tool.name()
    );
    Ok(())
}
