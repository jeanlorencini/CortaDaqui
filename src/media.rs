use std::{
    ffi::OsString,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use image::{ImageFormat, ImageReader, Limits, RgbImage};
use serde::Deserialize;
use tempfile::{Builder, TempPath};

use crate::{
    range::{ClipRange, YoutubeLink, seconds},
    tools::Binaries,
    worker::{Job, run},
};

const FORMAT: &str = "bv[ext=mp4][height<=1080]+ba[ext=m4a]/b[ext=mp4][height<=1080]";
const FALLBACK_FORMAT: &str = "bv*[height<=1080]+ba/b[height<=1080]";
const ENCODER: &[&str] = &[
    "-c:v",
    "libx264",
    "-preset",
    "veryfast",
    "-crf",
    "20",
    "-pix_fmt",
    "yuv420p",
    "-vf",
    "pad=ceil(iw/2)*2:ceil(ih/2)*2",
    "-threads",
    "4",
    "-fps_mode",
    "vfr",
    "-c:a",
    "aac",
    "-b:a",
    "160k",
    "-movflags",
    "+faststart",
    "-map_metadata",
    "-1",
    "-map_chapters",
    "-1",
    "-nostats",
    "-progress",
    "pipe:1",
];

#[derive(Clone, Debug)]
pub struct Video {
    pub link: YoutubeLink,
    pub title: String,
    pub duration_ms: i32,
    pub frame_ms: i32,
}

#[derive(Deserialize)]
struct Metadata {
    title: String,
    duration: Option<f64>,
    fps: Option<f64>,
    is_live: Option<bool>,
    live_status: Option<String>,
}

pub fn load(
    tools: &Binaries,
    http: &ureq::Agent,
    link: YoutubeLink,
    job: &Job,
) -> Result<(Video, Option<RgbImage>)> {
    job.progress("Buscando informações do vídeo…", None);
    let mut command = youtube(tools);
    command.args([
        "--format",
        &format!("{FORMAT}/{FALLBACK_FORMAT}"),
        "--skip-download",
        "--print",
        "%(.{title,duration,fps,is_live,live_status})j",
        "--",
        &link.canonical(),
    ]);
    let text = run(command, job, |_| {})
        .context("Não foi possível carregar o vídeo. Confira o link e a conexão.")?;
    let metadata: Metadata =
        serde_json::from_str(text.trim()).context("O vídeo não retornou informações legíveis.")?;
    ensure!(
        metadata.is_live != Some(true) && metadata.live_status.as_deref() != Some("is_upcoming"),
        "Aguarde o fim da transmissão para recortar o vídeo."
    );
    let duration = metadata
        .duration
        .filter(|duration| (0.001..=f64::from(i32::MAX) / 1000.0).contains(duration))
        .context("A duração deste vídeo não está disponível.")?;
    let fps = metadata
        .fps
        .filter(|fps| fps.is_finite() && *fps > 0.0)
        .unwrap_or(30.0);
    let thumbnail = thumbnail(http, &link.id).ok();
    job.check()?;
    Ok((
        Video {
            link,
            title: metadata.title,
            duration_ms: (duration * 1000.0).round() as i32,
            frame_ms: (1000.0 / fps).ceil().max(1.0) as i32,
        },
        thumbnail,
    ))
}

fn thumbnail(http: &ureq::Agent, id: &str) -> Result<RgbImage> {
    let bytes = http
        .get(&format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"))
        .config()
        .timeout_global(Some(Duration::from_secs(6)))
        .build()
        .call()?
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_vec()?;
    let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(1280);
    limits.max_image_height = Some(720);
    reader.limits(limits);
    Ok(reader.decode()?.into_rgb8())
}

fn youtube(tools: &Binaries) -> Command {
    let mut runtime = OsString::from("deno:");
    runtime.push(&tools.deno);
    let mut command = Command::new(&tools.yt_dlp);
    command
        .args([
            "--ignore-config",
            "--no-plugin-dirs",
            "--no-playlist",
            "--no-cache-dir",
            "--encoding",
            "utf-8",
            "--socket-timeout",
            "12",
            "--retries",
            "2",
            "--fragment-retries",
            "2",
            "--no-js-runtimes",
            "--js-runtimes",
        ])
        .arg(runtime)
        .arg("--ffmpeg-location")
        .arg(&tools.ffmpeg_dir);
    command
}

pub fn export(
    tools: &Binaries,
    video: &Video,
    range: ClipRange,
    folder: &Path,
    job: &Job,
) -> Result<PathBuf> {
    let workspace = Builder::new()
        .prefix(".cortadaqui-")
        .tempdir_in(folder)
        .context("Não foi possível gravar na pasta de destino.")?;
    let partial_path = workspace.path().join("trecho.mp4");
    let mut partial = youtube(tools);
    let length = seconds(range.length());
    let args = format!("ffmpeg_o:-t {length} {}", ENCODER.join(" "));
    partial
        .args([
            "--format",
            FORMAT,
            "--merge-output-format",
            "mp4",
            "--force-keyframes-at-cuts",
            "--download-sections",
            &format!("*{}-{}", seconds(range.start), seconds(range.end)),
            "--downloader-args",
            "ffmpeg_i:-rw_timeout 15000000",
            "--downloader-args",
            &args,
            "--output",
        ])
        .arg(workspace.path().join("trecho.%(ext)s"))
        .args(["--", &video.link.canonical()]);
    let label = "Baixando e codificando o trecho…";
    job.progress(label, None);
    let partial = run(partial, job, |line| {
        encode_progress(job, label, line, range)
    })
    .and_then(|_| verify(tools, &partial_path, range, job));

    let output = if partial.is_ok() {
        partial_path
    } else {
        job.check()?;
        let source = download_full(tools, video, workspace.path(), job)?;
        let output = workspace.path().join("resultado.mp4");
        recode(tools, &source, &output, range, job)?;
        verify(tools, &output, range, job)?;
        output
    };
    job.check()?;
    publish(output, folder, &video.title, range)
}

fn download_full(tools: &Binaries, video: &Video, workspace: &Path, job: &Job) -> Result<PathBuf> {
    let mut command = youtube(tools);
    let label = "Baixando o vídeo completo para recortar…";
    command
        .args([
            "--format",
            FALLBACK_FORMAT,
            "--merge-output-format",
            "mkv",
            "--remux-video",
            "mkv",
            "--newline",
            "--progress",
            "--progress-template",
            "download:CD_DOWNLOAD:%(progress._percent_str)s",
            "--output",
        ])
        .arg(workspace.join("origem.%(ext)s"))
        .args(["--", &video.link.canonical()]);
    job.progress(label, None);
    run(command, job, |line| {
        if let Some(percent) = line
            .strip_prefix("CD_DOWNLOAD:")
            .and_then(|n| n.trim().strip_suffix('%'))
            .and_then(|n| n.parse::<f32>().ok())
        {
            job.progress(label, Some(percent / 100.0));
        }
    })
    .context("Não foi possível baixar o vídeo. Confira a conexão e se ele está disponível.")?;
    Ok(workspace.join("origem.mkv"))
}

fn recode(
    tools: &Binaries,
    source: &Path,
    output: &Path,
    range: ClipRange,
    job: &Job,
) -> Result<()> {
    let mut command = Command::new(tools.ffmpeg_dir.join("ffmpeg.exe"));
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-ss",
            &seconds(range.start),
            "-i",
        ])
        .arg(source)
        .args([
            "-t",
            &seconds(range.length()),
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
        ])
        .args(ENCODER)
        .arg(output);
    let label = "Codificando o MP4…";
    job.progress(label, Some(0.0));
    run(command, job, |line| {
        encode_progress(job, label, line, range)
    })
    .context("O FFmpeg não conseguiu codificar o trecho.")?;
    Ok(())
}

fn encode_progress(job: &Job, label: &str, line: &str, range: ClipRange) {
    if let Some(micros) = line
        .strip_prefix("out_time_us=")
        .and_then(|n| n.parse::<f32>().ok())
    {
        job.progress(label, Some(micros / (range.length() as f32 * 1000.0)));
    }
}

#[derive(Deserialize)]
struct Probe {
    streams: Vec<Stream>,
    format: Container,
}

#[derive(Deserialize)]
struct Stream {
    codec_name: String,
    codec_type: String,
    avg_frame_rate: String,
    r_frame_rate: String,
}

#[derive(Deserialize)]
struct Container {
    duration: String,
}

fn verify(tools: &Binaries, output: &Path, range: ClipRange, job: &Job) -> Result<()> {
    let mut command = Command::new(tools.ffmpeg_dir.join("ffprobe.exe"));
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type,avg_frame_rate,r_frame_rate:format=duration",
            "-of",
            "json",
        ])
        .arg(output);
    let report: Probe = serde_json::from_str(&run(command, job, |_| {})?)
        .context("O MP4 gerado não pôde ser validado.")?;
    let video = report
        .streams
        .iter()
        .find(|stream| stream.codec_type == "video" && stream.codec_name == "h264")
        .context("O arquivo gerado não tem vídeo H.264.")?;
    ensure!(
        report
            .streams
            .iter()
            .all(|stream| stream.codec_type != "audio" || stream.codec_name == "aac"),
        "O áudio do arquivo gerado não está em AAC."
    );
    let fps = fps(&video.avg_frame_rate)
        .or_else(|| fps(&video.r_frame_rate))
        .context("O arquivo gerado não tem taxa de quadros válida.")?;
    let duration: f64 = report.format.duration.parse()?;
    ensure!(
        duration > 0.0
            && (duration - f64::from(range.length()) / 1000.0).abs() <= (1.0 / fps) + 0.002,
        "A duração do MP4 ficou fora da tolerância de um quadro."
    );
    Ok(())
}

fn fps(value: &str) -> Option<f64> {
    let (n, d) = value.split_once('/')?;
    let v = n.parse::<f64>().ok()? / d.parse::<f64>().ok()?;
    (v.is_finite() && v > 0.0).then_some(v)
}

fn publish(output: PathBuf, folder: &Path, title: &str, range: ClipRange) -> Result<PathBuf> {
    let title: String = title
        .chars()
        .take(64)
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let title = title.trim_matches([' ', '.']);
    let title = if title.is_empty() { "Trecho" } else { title };
    let stem = format!(
        "CortaDaqui - {title} - {}s a {}s",
        seconds(range.start),
        seconds(range.end)
    );
    let mut file = TempPath::try_from_path(output)?;
    for index in 1..=1000 {
        let filename = if index == 1 {
            format!("{stem}.mp4")
        } else {
            format!("{stem} ({index}).mp4")
        };
        let path = folder.join(filename);
        match file.persist_noclobber(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => file = error.path,
            Err(error) => {
                return Err(error).context("Não foi possível salvar o MP4 na pasta escolhida.");
            }
        }
    }
    anyhow::bail!("Há arquivos demais com este nome na pasta escolhida.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    #[test]
    fn publish_keeps_existing_exports() {
        let folder = tempfile::tempdir().unwrap();
        let range = ClipRange { start: 0, end: 400 };
        let stage = folder.path().join("stage.mp4");
        fs::write(&stage, "first").unwrap();
        let first = publish(stage.clone(), folder.path(), "Ficheiro: teste?", range).unwrap();
        fs::write(&stage, "second").unwrap();
        let second = publish(stage, folder.path(), "Ficheiro: teste?", range).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(first).unwrap(), "first");
        assert_eq!(fs::read_to_string(second).unwrap(), "second");
    }

    #[test]
    #[ignore = "requer FFmpeg e ffprobe no diretório CORTADAQUI_TEST_FFMPEG"]
    fn ffmpeg_starts_after_a_non_keyframe_cut_and_keeps_audio() -> Result<()> {
        let tools = Binaries {
            yt_dlp: PathBuf::new(),
            ffmpeg_dir: std::env::var_os("CORTADAQUI_TEST_FFMPEG")
                .context("CORTADAQUI_TEST_FFMPEG")?
                .into(),
            deno: PathBuf::new(),
        };
        let job = Job::new(Arc::new(AtomicBool::new(false)), Box::new(|_, _| {}));
        let sandbox = tempfile::tempdir()?;
        let source = sandbox.path().join("source.mp4");
        let result = sandbox.path().join("result.mp4");
        let pixel = sandbox.path().join("first.rgb");
        let mut ffmpeg = Command::new(tools.ffmpeg_dir.join("ffmpeg.exe"));
        ffmpeg
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=red:s=96x64:r=30:d=3,drawbox=c=green:t=fill:enable='eq(n,32)'",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=3",
                "-c:v",
                "libx264",
                "-g",
                "120",
                "-sc_threshold",
                "0",
                "-c:a",
                "aac",
            ])
            .arg(&source);
        run(ffmpeg, &job, |_| {})?;
        let range = ClipRange {
            start: 1040,
            end: 1800,
        };
        recode(&tools, &source, &result, range, &job)?;
        verify(&tools, &result, range, &job)?;

        let mut ffmpeg = Command::new(tools.ffmpeg_dir.join("ffmpeg.exe"));
        ffmpeg
            .args(["-v", "error", "-i"])
            .arg(&result)
            .args([
                "-an",
                "-frames:v",
                "1",
                "-vf",
                "scale=1:1",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
            ])
            .arg(&pixel);
        run(ffmpeg, &job, |_| {})?;
        let rgb = fs::read(pixel)?;
        assert_eq!(rgb.len(), 3);
        assert!(rgb[1] > 100 && rgb[0] < 30 && rgb[2] < 30, "{rgb:?}");

        let mut ffprobe = Command::new(tools.ffmpeg_dir.join("ffprobe.exe"));
        ffprobe
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "csv=p=0",
            ])
            .arg(&result);
        assert_eq!(run(ffprobe, &job, |_| {})?.trim(), "aac");
        Ok(())
    }

    #[test]
    #[ignore = "usa ferramentas instaladas e baixa um trecho público do YouTube"]
    fn exports_a_public_youtube_clip_and_can_cancel() -> Result<()> {
        let root = std::env::var_os("CORTADAQUI_TEST_TOOLS").context("CORTADAQUI_TEST_TOOLS")?;
        let toolkit = crate::tools::Toolkit::new(root.into());
        let tools = toolkit
            .binaries()
            .context("Instale as ferramentas antes de rodar o teste.")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let job = Job::new(
            Arc::clone(&cancel),
            Box::new(|stage, _| eprintln!("{stage}")),
        );
        let link = YoutubeLink::parse("https://youtu.be/aqz-KE-bpKQ?t=95")?;
        let (video, thumb) = load(&tools, toolkit.http(), link, &job)?;
        assert_eq!(video.link.start_ms, Some(95_000));
        assert!(video.title.contains("Big Buck Bunny"));
        assert!(thumb.is_some());
        let folder = tempfile::tempdir()?;
        let range = ClipRange {
            start: 95_025,
            end: 97_078,
        };
        let clip = export(&tools, &video, range, folder.path(), &job)?;
        assert!(clip.is_file());
        verify(&tools, &clip, range, &job)?;

        let cancel_from_progress = Arc::clone(&cancel);
        let cancelling_job = Job::new(
            cancel,
            Box::new(move |_, fraction| {
                if fraction.is_some_and(|value| value > 0.0) {
                    cancel_from_progress.store(true, Ordering::Relaxed);
                }
            }),
        );
        let long_range = ClipRange {
            start: 95_025,
            end: 151_000,
        };
        let err = export(&tools, &video, long_range, folder.path(), &cancelling_job).unwrap_err();
        assert!(err.is::<crate::worker::Cancelled>(), "{err:#}");
        assert_eq!(fs::read_dir(folder.path())?.count(), 1);
        Ok(())
    }
}
