#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod media;
mod range;
mod tools;
mod worker;

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Result, anyhow};
use slint::{ComponentHandle, Image, SharedPixelBuffer, Timer, TimerMode};

use crate::{
    media::Video,
    range::{ClipRange, YoutubeLink, clock},
    tools::{Binaries, PendingRelease, Tool, Toolkit},
    worker::{Cancelled, Job},
};

slint::include_modules!();

struct State {
    tools: Toolkit,
    available: [Option<PendingRelease>; 2],
    video: Option<Video>,
    active: Option<Active>,
    job_timer: Timer,
    clipboard_timer: Timer,
    clipboard_sequence: u32,
}

struct Active {
    cancel: Arc<AtomicBool>,
    receiver: Receiver<Update>,
    thread: JoinHandle<()>,
}

enum Task {
    Setup,
    Check(usize),
    Install(usize, Option<PendingRelease>),
    Load(Binaries, YoutubeLink),
    Export(Binaries, Video, ClipRange, PathBuf),
}

enum Outcome {
    Setup,
    Checked(usize, PendingRelease),
    Installed(usize),
    Loaded(Video, Option<image::RgbImage>),
    Saved(PathBuf),
}

enum Update {
    Progress(String, Option<f32>),
    Done(Result<Outcome>),
}

const TOOLS: [Tool; 2] = [Tool::YtDlp, Tool::Ffmpeg];

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    let weak = ui.as_weak();
    ui.on_minimize_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let weak = ui.as_weak();
    ui.on_toggle_window_size(move || {
        if let Some(ui) = weak.upgrade() {
            let window = ui.window();
            window.set_maximized(!window.is_maximized());
        }
    });
    ui.on_close_window(|| {
        let _ = slint::quit_event_loop();
    });
    let Some(profile) = std::env::var_os("LOCALAPPDATA") else {
        status(
            &ui,
            "Não foi possível localizar a pasta local do usuário.",
            "",
            1,
        );
        return ui.run();
    };
    let tools = Toolkit::new(PathBuf::from(profile).join("CortaDaqui").join("tools"));
    let state = Rc::new(RefCell::new(State {
        tools,
        available: [None, None],
        video: None,
        active: None,
        job_timer: Timer::default(),
        clipboard_timer: Timer::default(),
        clipboard_sequence: 0,
    }));
    connect(&ui, &state);
    refresh_tools(&ui, &state.borrow());
    if ui.get_ready() {
        status(&ui, "Carregue um vídeo para começar.", "", 0);
    } else {
        launch(&ui, &state, Task::Setup);
    }
    let result = ui.run();
    if let Some(active) = state.borrow_mut().active.take() {
        active.cancel.store(true, Ordering::Relaxed);
        let _ = active.thread.join();
    }
    result
}

fn connect(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_load_link(move || {
        if let Some(ui) = weak.upgrade() {
            load_link(&ui, &app);
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_link_edited(move || {
        if let Some(ui) = weak.upgrade() {
            reset_video(&ui, &mut app.borrow_mut());
            if ui.get_ready() {
                status(&ui, "Carregue um vídeo para começar.", "", 0);
            }
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_times_edited(move || {
        if let Some(ui) = weak.upgrade() {
            validate_range(&ui, &app.borrow());
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_seek(move |handle, time| {
        let Some(ui) = weak.upgrade() else { return };
        let state = app.borrow();
        let Some(video) = &state.video else { return };
        let (mut start, mut end) = (ui.get_start_ms(), ui.get_end_ms());
        let frame = video.frame_ms.min(video.duration_ms);
        if handle == 0 {
            start = time.clamp(0, end - frame);
        } else {
            end = time.clamp(start + frame, video.duration_ms);
        }
        select_range(&ui, ClipRange { start, end });
    });
    let weak = ui.as_weak();
    ui.on_choose_folder(move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut dialog = rfd::FileDialog::new().set_title("Escolher pasta para salvar o MP4");
        let folder = destination(&ui);
        if folder.is_absolute() {
            dialog = dialog.set_directory(folder);
        }
        if let Some(folder) = dialog.pick_folder() {
            ui.set_folder(folder.to_string_lossy().as_ref().into());
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_export_clip(move || {
        let Some(ui) = weak.upgrade() else { return };
        let task = {
            let state = app.borrow();
            let Some(range) = validate_range(&ui, &state) else {
                return;
            };
            let folder = destination(&ui);
            if !folder.is_absolute() {
                status(
                    &ui,
                    "Escolha a pasta ou digite um caminho completo para o MP4.",
                    "",
                    1,
                );
                return;
            }
            let Some(tools) = state.tools.binaries() else {
                refresh_tools(&ui, &state);
                status(
                    &ui,
                    "Uma ferramenta ficou indisponível. Tente instalá-la novamente.",
                    "",
                    1,
                );
                return;
            };
            Task::Export(tools, state.video.as_ref().unwrap().clone(), range, folder)
        };
        launch(&ui, &app, task);
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_cancel(move || {
        if let (Some(ui), Some(active)) = (weak.upgrade(), app.borrow().active.as_ref()) {
            active.cancel.store(true, Ordering::Relaxed);
            ui.set_cancelling(true);
            status(&ui, "Cancelando a exportação…", "", 0);
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_prepare_tools(move || {
        if let Some(ui) = weak.upgrade() {
            launch(&ui, &app, Task::Setup);
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_check_tool(move |index| {
        if let Some(ui) = weak.upgrade() {
            launch(&ui, &app, Task::Check(index as usize));
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_install_tool(move |index| {
        if let Some(ui) = weak.upgrade() {
            let index = index as usize;
            let release = app.borrow().available[index].clone();
            launch(&ui, &app, Task::Install(index, release));
        }
    });
    let weak = ui.as_weak();
    let app = Rc::clone(state);
    ui.on_detection_toggled(move || {
        if let Some(ui) = weak.upgrade() {
            toggle_clipboard(&ui, &app);
        }
    });
}

fn launch(ui: &AppWindow, state: &Rc<RefCell<State>>, task: Task) {
    let mut app = state.borrow_mut();
    if app.active.is_some() {
        return;
    }
    let label = match &task {
        Task::Setup => "Preparando ferramentas…".to_owned(),
        Task::Check(index) => format!("Verificando {}…", TOOLS[*index].name()),
        Task::Install(index, _) => format!("Preparando {}…", TOOLS[*index].name()),
        Task::Load(..) => "Carregando o vídeo…".to_owned(),
        Task::Export(..) => "Preparando a exportação…".to_owned(),
    };
    status(ui, &label, "", 0);
    ui.set_busy(true);
    ui.set_progress(-1.0);
    ui.set_exporting(matches!(task, Task::Export(..)));
    let toolkit = app.tools.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let thread = thread::spawn(move || {
        let reporter = sender.clone();
        let job = Job::new(
            worker_cancel,
            Box::new(move |stage, fraction| {
                let _ = reporter.send(Update::Progress(stage.to_owned(), fraction));
            }),
        );
        let _ = sender.send(Update::Done(execute(&toolkit, task, &job)));
    });
    app.active = Some(Active {
        cancel,
        receiver,
        thread,
    });
    let weak = ui.as_weak();
    let state = Rc::downgrade(state);
    app.job_timer
        .start(TimerMode::Repeated, Duration::from_millis(85), move || {
            if let (Some(ui), Some(state)) = (weak.upgrade(), state.upgrade()) {
                poll_job(&ui, &mut state.borrow_mut());
            }
        });
}

fn execute(toolkit: &Toolkit, task: Task, job: &Job) -> Result<Outcome> {
    match task {
        Task::Setup => {
            let yt = toolkit.prepare(Tool::YtDlp, job);
            job.check()?;
            let ffmpeg = toolkit.prepare(Tool::Ffmpeg, job);
            yt.and(ffmpeg)?;
            Ok(Outcome::Setup)
        }
        Task::Check(index) => Ok(Outcome::Checked(index, toolkit.latest(TOOLS[index])?)),
        Task::Install(index, release) => {
            if let Some(release) = release {
                if matches!(TOOLS[index], Tool::YtDlp) {
                    toolkit.install(&toolkit.latest(Tool::Deno)?, job)?;
                }
                toolkit.install(&release, job)?;
            } else {
                toolkit.prepare(TOOLS[index], job)?;
            }
            Ok(Outcome::Installed(index))
        }
        Task::Load(tools, link) => {
            let (video, thumbnail) = media::load(&tools, toolkit.http(), link, job)?;
            Ok(Outcome::Loaded(video, thumbnail))
        }
        Task::Export(tools, video, range, folder) => Ok(Outcome::Saved(media::export(
            &tools, &video, range, &folder, job,
        )?)),
    }
}

fn poll_job(ui: &AppWindow, state: &mut State) {
    let Some(active) = &state.active else { return };
    let outcome = loop {
        match active.receiver.try_recv() {
            Ok(Update::Progress(label, fraction)) => {
                if !ui.get_cancelling() {
                    ui.set_status(label.into());
                    ui.set_progress(fraction.unwrap_or(-1.0));
                }
            }
            Ok(Update::Done(result)) => break result,
            Err(TryRecvError::Disconnected) => {
                break Err(anyhow!("A operação foi interrompida inesperadamente."));
            }
            Err(TryRecvError::Empty) => return,
        }
    };
    let _ = state.active.take().unwrap().thread.join();
    state.job_timer.stop();
    ui.set_busy(false);
    ui.set_exporting(false);
    ui.set_cancelling(false);
    ui.set_progress(-1.0);
    refresh_tools(ui, state);
    match outcome {
        Ok(Outcome::Setup) => status(
            ui,
            "Ferramentas prontas. Carregue um vídeo para começar.",
            "",
            2,
        ),
        Ok(Outcome::Checked(index, release)) => {
            if state.tools.has_release(&release) {
                state.available[index] = None;
                status(
                    ui,
                    &format!("{} está atualizado.", TOOLS[index].name()),
                    "",
                    2,
                );
            } else {
                state.available[index] = Some(release);
                status(
                    ui,
                    &format!("Há uma atualização de {} disponível.", TOOLS[index].name()),
                    "",
                    0,
                );
            }
            refresh_tools(ui, state);
        }
        Ok(Outcome::Installed(index)) => {
            state.available[index] = None;
            refresh_tools(ui, state);
            status(
                ui,
                &format!("{} está pronto para uso.", TOOLS[index].name()),
                "",
                2,
            );
        }
        Ok(Outcome::Loaded(video, thumbnail)) => {
            let frame = video.frame_ms.min(video.duration_ms);
            let start = video
                .link
                .start_ms
                .filter(|&time| time <= video.duration_ms - frame)
                .unwrap_or(0);
            let end = video.duration_ms;
            ui.set_total_ms(end);
            ui.set_total_time(clock(end).into());
            ui.set_video_title(video.title.as_str().into());
            if let Some(image) = thumbnail {
                let pixels = SharedPixelBuffer::clone_from_slice(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                );
                ui.set_thumbnail(Image::from_rgb8(pixels));
                ui.set_has_thumbnail(true);
            }
            select_range(ui, ClipRange { start, end });
            let detail = if video.link.start_ms.is_some_and(|time| time != start) {
                "A marca de tempo do link está fora do intervalo disponível."
            } else {
                ""
            };
            status(
                ui,
                "Ajuste o trecho e escolha onde salvar o MP4.",
                detail,
                0,
            );
            state.video = Some(video);
        }
        Ok(Outcome::Saved(path)) => status(ui, "MP4 salvo.", &path.to_string_lossy(), 2),
        Err(error) if error.is::<Cancelled>() => status(ui, "Exportação cancelada.", "", 0),
        Err(error) => show_error(ui, &error),
    }
}

fn refresh_tools(ui: &AppWindow, state: &State) {
    ui.set_ready(state.tools.binaries().is_some());
    ui.set_yt_version(state.tools.version(Tool::YtDlp).unwrap_or_default().into());
    ui.set_ffmpeg_version(state.tools.version(Tool::Ffmpeg).unwrap_or_default().into());
    let available = |index: usize| {
        state.available[index]
            .as_ref()
            .map_or("", PendingRelease::version)
            .into()
    };
    ui.set_yt_available(available(0));
    ui.set_ffmpeg_available(available(1));
}

fn load_link(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    if ui.get_busy() {
        return;
    }
    let link = match YoutubeLink::parse(ui.get_link().as_str()) {
        Ok(link) => link,
        Err(error) => {
            show_error(ui, &error);
            return;
        }
    };
    let Some(tools) = state.borrow().tools.binaries() else {
        refresh_tools(ui, &state.borrow());
        status(ui, "Aguarde a instalação das ferramentas.", "", 1);
        return;
    };
    reset_video(ui, &mut state.borrow_mut());
    launch(ui, state, Task::Load(tools, link));
}

fn reset_video(ui: &AppWindow, state: &mut State) {
    state.video = None;
    ui.set_total_ms(0);
    ui.set_total_time(clock(0).into());
    ui.set_video_title("".into());
    ui.set_has_thumbnail(false);
    ui.set_thumbnail(Image::default());
    select_range(ui, ClipRange { start: 0, end: 0 });
}

fn destination(ui: &AppWindow) -> PathBuf {
    PathBuf::from(ui.get_folder().as_str().trim().trim_matches('"'))
}

fn select_range(ui: &AppWindow, range: ClipRange) {
    ui.set_start_text(clock(range.start).into());
    ui.set_end_text(clock(range.end).into());
    draw_range(ui, range);
}

fn draw_range(ui: &AppWindow, range: ClipRange) {
    ui.set_start_ms(range.start);
    ui.set_end_ms(range.end);
    ui.set_clip_time(clock(range.length()).into());
    ui.set_range_error("".into());
}

fn validate_range(ui: &AppWindow, state: &State) -> Option<ClipRange> {
    let video = state.video.as_ref()?;
    match ClipRange::from_text(
        ui.get_start_text().as_str(),
        ui.get_end_text().as_str(),
        video.duration_ms,
        video.frame_ms.min(video.duration_ms),
    ) {
        Ok(range) => {
            draw_range(ui, range);
            Some(range)
        }
        Err(error) => {
            ui.set_range_error(error.to_string().into());
            None
        }
    }
}

fn toggle_clipboard(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let mut app = state.borrow_mut();
    app.clipboard_timer.stop();
    if !ui.get_detect_links() {
        return;
    }
    app.clipboard_sequence = clipboard_sequence();
    let weak = ui.as_weak();
    let state = Rc::downgrade(state);
    app.clipboard_timer
        .start(TimerMode::Repeated, Duration::from_millis(700), move || {
            let (Some(ui), Some(state)) = (weak.upgrade(), state.upgrade()) else {
                return;
            };
            if ui.get_busy() || !ui.get_ready() {
                return;
            }
            let sequence = clipboard_sequence();
            if sequence == state.borrow().clipboard_sequence {
                return;
            }
            let text = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text());
            if matches!(text, Err(arboard::Error::ClipboardOccupied)) {
                return;
            }
            state.borrow_mut().clipboard_sequence = sequence;
            let Ok(text) = text else { return };
            let link = text.trim();
            if link.len() <= 4096
                && link != ui.get_link().as_str()
                && YoutubeLink::parse(link).is_ok()
            {
                ui.set_link(link.into());
                ui.set_tools_page(false);
                load_link(&ui, &state);
            }
        });
}

fn clipboard_sequence() -> u32 {
    #[cfg(windows)]
    {
        // O Windows fornece um contador de alterações sem abrir a área de transferência.
        unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn status(ui: &AppWindow, text: &str, detail: &str, kind: i32) {
    ui.set_status(text.into());
    ui.set_status_detail(detail.into());
    ui.set_status_kind(kind);
}

fn show_error(ui: &AppWindow, error: &anyhow::Error) {
    let message = error.to_string();
    let detail = error
        .chain()
        .nth(1)
        .map(ToString::to_string)
        .unwrap_or_default();
    status(ui, &message, &detail, 1);
}
