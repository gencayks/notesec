//! Voice notes in the app (decision 52): the recorder running while the
//! "REC" pill shows (Stop / Cancel), the note block it leaves, its Play
//! and Transcribe buttons in reading view, transcriptions done one at a
//! time off the UI thread, and Settings > Voice notes. The work itself is
//! in `crate::voice`. The pill is not a dialog: typing, vim keys and
//! every shortcut keep working while recording.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, MouseButton, PathPromptOptions, Task, Window,
};
use uuid::Uuid;

use super::{
    create_journal, CancelRecording, NoteSec, RecordVoiceNote, Status, StopRecording,
    TranscribeVoiceNotes,
};
use crate::assets::{parse_images, resolve, ImageRef};
use crate::model::{find_block, Block};
use crate::storage::today_title;
use crate::voice::private::PrivateDir;
use crate::voice::run;
use crate::voice::whisper::{self, Whisper};
use crate::voice::{self, WavInfo};

/// How often the pill's clock moves (and the recorder is checked on).
const TICK: Duration = Duration::from_secs(1);
/// How long a recorder gets to close its file after SIGINT.
const GRACE: Duration = Duration::from_secs(5);
/// The block content that marks a transcript (a child of its note).
pub(super) const TRANSCRIPT: &str = "**Transcript:** ";

/// The recorder while it runs.
struct Recording {
    /// `None` once handed on to be stopped.
    child: Option<Child>,
    /// Where the recorder writes: a private folder (0700, removed with
    /// the recording) outside the vault, so other users can't read it
    /// and a git backup during the recording never commits half a file.
    temp: PathBuf,
    /// The name it gets in `assets/` once finished, and that path.
    file: String,
    path: PathBuf,
    started: Instant,
    /// The page the note goes on, and the block it goes below.
    page: String,
    anchor: Option<Uuid>,
    /// The recorder's output (shown if it fails).
    log: PathBuf,
    /// Holds `temp` and `log`; dropped (deleted) after `Drop` below.
    _dir: PrivateDir,
}

impl Drop for Recording {
    /// Closing the app while recording: the recording is finished and kept
    /// in `assets/` (no block points at it).
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            run::interrupt(&mut child, std::env::var_os("PATH").as_deref(), GRACE);
            if voice::finish_wav(&self.temp).is_ok() {
                let _ = voice::store(&self.temp, &self.path);
            }
        }
    }
}

pub(super) struct VoiceState {
    recording: Option<Recording>,
    /// Time recorded so far (the pill's clock), updated each tick.
    pub(super) elapsed: Duration,
    tick: Option<Task<()>>,
    /// Stopping the recorder and checking the file.
    finishing: Option<Task<()>>,
    /// The note blocks waiting to be transcribed, and the one being done.
    queue: VecDeque<Uuid>,
    transcribing: Option<(Uuid, Task<()>)>,
    /// Settings' "Test" result and the check running.
    pub(super) check: Option<Result<String, String>>,
    check_task: Option<Task<()>>,
    /// The recording stops by itself after this (30 minutes).
    pub(super) max_length: Duration,
    /// Tests: `PATH` for finding a recorder, instead of the real one.
    #[cfg(test)]
    pub(super) path_var: Option<OsString>,
    /// Tests: the files Play opened (the test platform can't).
    #[cfg(test)]
    pub(super) opened: Vec<PathBuf>,
}

impl Default for VoiceState {
    fn default() -> Self {
        VoiceState {
            recording: None,
            elapsed: Duration::ZERO,
            tick: None,
            finishing: None,
            queue: VecDeque::new(),
            transcribing: None,
            check: None,
            check_task: None,
            max_length: voice::MAX_LENGTH,
            #[cfg(test)]
            path_var: None,
            #[cfg(test)]
            opened: Vec::new(),
        }
    }
}

impl NoteSec {
    pub(super) fn recording(&self) -> bool {
        self.voice.recording.is_some()
    }

    /// The file being recorded into (tests).
    #[cfg(test)]
    pub(super) fn recording_file(&self) -> Option<PathBuf> {
        self.voice.recording.as_ref().map(|r| r.temp.clone())
    }

    fn voice_path_var(&self) -> Option<OsString> {
        #[cfg(test)]
        if let Some(path) = &self.voice.path_var {
            return Some(path.clone());
        }
        std::env::var_os("PATH")
    }

    fn whisper(&self) -> Option<Whisper> {
        (!self.config.whisper_binary.trim().is_empty()).then(|| Whisper {
            binary: PathBuf::from(self.config.whisper_binary.trim()),
            model: PathBuf::from(self.config.whisper_model.trim()),
            language: self.config.whisper_language.clone(),
        })
    }

    pub(super) fn on_record_voice_note(
        &mut self,
        _: &RecordVoiceNote,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recording() {
            self.stop_recording(cx);
        } else {
            self.start_recording(cx);
        }
    }

    pub(super) fn on_stop_recording(
        &mut self,
        _: &StopRecording,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_recording(cx);
    }

    pub(super) fn on_cancel_recording(
        &mut self,
        _: &CancelRecording,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_recording(cx);
    }

    pub(super) fn on_transcribe_voice_notes(
        &mut self,
        _: &TranscribeVoiceNotes,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self.current_page() else {
            return;
        };
        let Some(ix) = self.find_page(&title) else {
            return;
        };
        let page = &self.pages[ix];
        let notes: Vec<Uuid> = (0..page.blocks.len())
            .filter(|&b| voice_target(&page.blocks[b].content).is_some())
            .filter(|&b| !has_transcript(&page.blocks, b))
            .map(|b| page.blocks[b].id)
            .collect();
        if notes.is_empty() {
            let text = "No voice notes without a transcript on this page".to_string();
            self.show_status(Status { text, error: false }, cx);
        }
        for id in notes {
            self.transcribe_note(id, cx);
        }
    }

    /// Start the recorder, writing `assets/voice-<now>.wav`. The note will
    /// go below the block being edited, else at the end of the page on
    /// screen (today's journal without one).
    pub(super) fn start_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording() || self.voice.finishing.is_some() {
            return;
        }
        let path_var = self.voice_path_var();
        let recorder = match voice::detect(&self.config.voice_recorder, path_var.as_deref()) {
            Ok(recorder) => recorder,
            Err(text) => return self.show_status(Status { text, error: true }, cx),
        };
        let assets = self.storage.root().join("assets");
        if let Err(err) = std::fs::create_dir_all(&assets) {
            let text = format!("Could not create {}: {err}", assets.display());
            return self.show_status(Status { text, error: true }, cx);
        }
        let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string();
        let file = voice::new_file_name(&assets, &stamp);
        let path = assets.join(&file);
        let dir = match PrivateDir::new() {
            Ok(dir) => dir,
            Err(err) => {
                let text = format!("Could not make a private folder to record into: {err}");
                return self.show_status(Status { text, error: true }, cx);
            }
        };
        let log = dir.join("recorder.log");
        let temp = dir.join("recording.wav");
        let child = match run::spawn(&recorder.program, &recorder.args(&temp), &log) {
            Ok(child) => child,
            Err(err) => {
                let text = format!("Could not start {}: {err}", recorder.describe());
                return self.show_status(Status { text, error: true }, cx);
            }
        };
        let page = self.current_page().unwrap_or_else(today_title);
        let anchor = self
            .editing
            .filter(|_| self.current_page().as_deref() == Some(page.as_str()))
            .and_then(|ix| self.pages[self.selected].blocks.get(ix))
            .map(|b| b.id);
        self.voice.recording = Some(Recording {
            child: Some(child),
            temp,
            file,
            path,
            started: Instant::now(),
            page,
            anchor,
            log,
            _dir: dir,
        });
        self.voice.elapsed = Duration::ZERO;
        self.voice.tick = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            match this.update(cx, |this, cx| this.recording_tick(cx)) {
                Ok(true) => {}
                _ => break,
            }
        }));
        let text = format!("Recording with {}", recorder.kind.program());
        self.show_status(Status { text, error: false }, cx);
        cx.notify();
    }

    /// Each second: move the clock, stop at the limit, notice a recorder
    /// that quit by itself. False once not recording.
    fn recording_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let max = self.voice.max_length;
        let Some(rec) = &mut self.voice.recording else {
            return false;
        };
        self.voice.elapsed = rec.started.elapsed();
        let exited = rec
            .child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
        if exited {
            self.finish_recording(Some("The recorder stopped by itself".into()), cx);
            return false;
        }
        if self.voice.elapsed >= max {
            let note = format!("Recording stopped at the {} limit", voice::clock(max));
            self.finish_recording(Some(note), cx);
            return false;
        }
        cx.notify();
        true
    }

    pub(super) fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.finish_recording(None, cx);
    }

    /// Stop the recorder (SIGINT), check and repair the WAV off the UI
    /// thread, then add the note block. `why` says why it stopped, if
    /// not by the user.
    fn finish_recording(&mut self, why: Option<String>, cx: &mut Context<Self>) {
        let Some(mut rec) = self.voice.recording.take() else {
            return;
        };
        self.voice.tick = None;
        let path_var = self.voice_path_var();
        let mut child = rec.child.take();
        let path = rec.path.clone();
        let temp = rec.temp.clone();
        self.show_status(
            Status {
                text: "Saving the voice note\u{2026}".into(),
                error: false,
            },
            cx,
        );
        self.voice.finishing = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if let Some(child) = &mut child {
                        run::interrupt(child, path_var.as_deref(), GRACE);
                    }
                    let checked = voice::finish_wav(&temp).and_then(|info| {
                        // A name taken meanwhile (another window): a new one.
                        if path.exists() {
                            return Err("a file with its name appeared in assets/".into());
                        }
                        voice::store(&temp, &path)
                            .map(|_| info)
                            .map_err(|e| format!("could not move it into assets/: {e}"))
                    });
                    let log = run::tail(&run::read_lossy(&rec.log));
                    (rec, checked, log)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.voice.finishing = None;
                let (rec, checked, log) = result;
                this.recording_finished(rec, checked, log, why, cx);
            });
        }));
        cx.notify();
    }

    fn recording_finished(
        &mut self,
        rec: Recording,
        checked: Result<WavInfo, String>,
        log: String,
        why: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let info = match checked {
            Ok(info) => info,
            Err(err) => {
                let mut text = format!("Voice note not saved: {err}");
                if let Some(why) = why {
                    text = format!("{why}. {text}");
                }
                if !log.is_empty() {
                    text.push_str(&format!(" ({log})"));
                }
                return self.show_status(Status { text, error: true }, cx);
            }
        };
        match self.insert_voice_note(&rec.page, rec.anchor, &rec.file, cx) {
            Ok((title, id)) => {
                let mut text = format!(
                    "Voice note saved ({}) on \u{201c}{title}\u{201d}",
                    voice::clock(info.duration())
                );
                if let Some(why) = why {
                    text = format!("{why}. {text}");
                }
                self.show_status(Status { text, error: false }, cx);
                if self.config.voice_auto_transcribe && self.whisper().is_some() {
                    self.transcribe_note(id, cx);
                }
            }
            Err(text) => self.show_status(Status { text, error: true }, cx),
        }
        cx.notify();
    }

    /// Add `![voice note](../assets/<file>)` below `anchor`'s subtree on
    /// `page` (at its end without one; today's journal if the page is
    /// gone). Returns the page's title and the new block's id.
    fn insert_voice_note(
        &mut self,
        page: &str,
        anchor: Option<Uuid>,
        file: &str,
        cx: &mut Context<Self>,
    ) -> Result<(String, Uuid), String> {
        let ix = match self.find_page(page) {
            Some(ix) => ix,
            None => {
                let day = today_title();
                if self.find_journal(&day).is_none() {
                    let journal = create_journal(&self.storage, &day);
                    self.add_page(journal);
                }
                self.find_journal(&day)
                    .ok_or("could not open today's journal")?
            }
        };
        self.stop_editing_page(ix, cx);
        let markdown = voice::note_markdown(file);
        let page = &mut self.pages[ix];
        let at = match anchor.and_then(|id| page.blocks.iter().position(|b| b.id == id)) {
            Some(a) => page.insert_after_subtree(a, markdown),
            None if page.blocks.len() == 1 && page.blocks[0].content.trim().is_empty() => {
                page.blocks[0].content = markdown;
                0
            }
            None => page.push_block(markdown),
        };
        let id = page.blocks[at].id;
        let title = page.title.clone();
        self.storage
            .save(&self.pages[ix])
            .map_err(|e| format!("Could not save \u{201c}{title}\u{201d}: {e}"))?;
        // Undo snapshots hold every page: one from before would drop it.
        self.forget_history();
        Ok((title, id))
    }

    /// Blocks on page `ix` are about to move: save and close the block
    /// being edited there (if any).
    fn stop_editing_page(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.selected == ix && self.editing.is_some() {
            self.stop_edit(cx);
        }
    }

    /// Stop recording and delete the file.
    pub(super) fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        let Some(mut rec) = self.voice.recording.take() else {
            return;
        };
        self.voice.tick = None;
        if let Some(mut child) = rec.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Dropping it deletes its folder, the recording with it.
        drop(rec);
        let text = "Recording cancelled".to_string();
        self.show_status(Status { text, error: false }, cx);
        cx.notify();
    }

    /// Open a voice note with the system's player.
    fn play_voice_note(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !path.is_file() {
            let text = format!("Voice note not found: {}", path.display());
            return self.show_status(Status { text, error: true }, cx);
        }
        #[cfg(not(test))]
        cx.open_with_system(&path);
        #[cfg(test)]
        self.voice.opened.push(path);
    }

    /// Queue the note block `id` for transcription.
    pub(super) fn transcribe_note(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.whisper().is_none() {
            let text = "Set a whisper program in Settings > Voice notes to transcribe".to_string();
            return self.show_status(Status { text, error: true }, cx);
        }
        let busy = self
            .voice
            .transcribing
            .as_ref()
            .is_some_and(|(b, _)| *b == id);
        if !busy && !self.voice.queue.contains(&id) {
            self.voice.queue.push_back(id);
        }
        self.next_transcription(cx);
    }

    fn next_transcription(&mut self, cx: &mut Context<Self>) {
        if self.voice.transcribing.is_some() {
            return;
        }
        let Some(whisper) = self.whisper() else {
            self.voice.queue.clear();
            return;
        };
        while let Some(id) = self.voice.queue.pop_front() {
            let Some(path) = self.voice_note_path(id) else {
                continue;
            };
            let waiting = self.voice.queue.len();
            let mut text = "Transcribing the voice note\u{2026}".to_string();
            if waiting > 0 {
                text.push_str(&format!(" ({waiting} more waiting)"));
            }
            self.show_status(Status { text, error: false }, cx);
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(async move {
                        let bytes = std::fs::read(&path)
                            .map_err(|e| format!("Could not read the voice note: {e}"))?;
                        let (_, info) = voice::normalize_wav(&bytes)
                            .map_err(|e| format!("Can't transcribe: {e}"))?;
                        whisper.transcribe(&path, whisper::timeout(info.duration()))
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.voice.transcribing = None;
                    this.transcription_done(id, result, cx);
                    this.next_transcription(cx);
                });
            });
            self.voice.transcribing = Some((id, task));
            break;
        }
        cx.notify();
    }

    /// The audio file of note block `id`.
    fn voice_note_path(&self, id: Uuid) -> Option<PathBuf> {
        let (p, b) = find_block(&self.pages, id)?;
        let target = voice_target(&self.pages[p].blocks[b].content)?;
        resolve(self.storage.root(), &target.target)
    }

    /// Add the transcript as a child of note block `id`.
    fn transcription_done(
        &mut self,
        id: Uuid,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let text = match result {
            Ok(text) => text,
            Err(text) => return self.show_status(Status { text, error: true }, cx),
        };
        if text.is_empty() {
            let text = "No speech found in the voice note".to_string();
            return self.show_status(Status { text, error: false }, cx);
        }
        let Some((p, b)) = find_block(&self.pages, id) else {
            let text = "The voice note was deleted before its transcript was ready".to_string();
            return self.show_status(Status { text, error: true }, cx);
        };
        if has_transcript(&self.pages[p].blocks, b) {
            let text = "The voice note already has a transcript".to_string();
            return self.show_status(Status { text, error: false }, cx);
        }
        self.stop_editing_page(p, cx);
        let page = &mut self.pages[p];
        let at = page.subtree_end(b);
        page.blocks.insert(
            at,
            Block {
                id: Uuid::new_v4(),
                // Spoken words can't make links, properties or bullets.
                content: format!("{TRANSCRIPT}{}", crate::clipper::html::defang(&text)),
                parent_id: Some(id),
                page_id: page.id.clone(),
                order: 0,
            },
        );
        page.renumber();
        let title = page.title.clone();
        if let Err(err) = self.storage.save(&self.pages[p]) {
            let text = format!("Could not save \u{201c}{title}\u{201d}: {err}");
            return self.show_status(Status { text, error: true }, cx);
        }
        self.forget_history();
        let text = format!("Transcribed a voice note on \u{201c}{title}\u{201d}");
        self.show_status(Status { text, error: false }, cx);
        cx.notify();
    }

    /// Settings: pick the whisper program (`model == false`) or model.
    fn choose_whisper_path(&mut self, model: bool, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                if model {
                    "Use this model"
                } else {
                    "Use this program"
                }
                .into(),
            ),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        this.set_whisper_path(model, path.display().to_string(), cx)
                    });
                }
            }
        })
        .detach();
    }

    pub(super) fn set_whisper_path(&mut self, model: bool, path: String, cx: &mut Context<Self>) {
        if model {
            self.config.whisper_model = path;
        } else {
            self.config.whisper_binary = path;
        }
        self.voice.check = None;
        self.save_config();
        cx.notify();
    }

    fn set_auto_transcribe(&mut self, on: bool, cx: &mut Context<Self>) {
        self.config.voice_auto_transcribe = on;
        self.save_config();
        cx.notify();
    }

    /// Settings' "Test": run the program's `--help` and look at the model.
    pub(super) fn test_whisper(&mut self, cx: &mut Context<Self>) {
        let binary = PathBuf::from(self.config.whisper_binary.trim());
        let model = PathBuf::from(self.config.whisper_model.trim());
        self.voice.check = Some(Ok("Testing\u{2026}".into()));
        self.voice.check_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { whisper::check(&binary, &model) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.voice.check = Some(result);
                this.voice.check_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The pill at the top right while recording (clock, Stop, Cancel) or
    /// transcribing. Not modal: keys go where they always go.
    pub(super) fn render_voice_pill(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = self.theme;
        let recording = self.recording();
        let label = if recording {
            format!(
                "\u{25cf} REC {} / {}",
                voice::clock(self.voice.elapsed),
                voice::clock(self.voice.max_length)
            )
        } else if self.voice.finishing.is_some() {
            "Saving voice note\u{2026}".to_string()
        } else if self.voice.transcribing.is_some() {
            "Transcribing\u{2026}".to_string()
        } else {
            return None;
        };
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|d| d.bg(theme.selected_bg))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        Some(
            div()
                .id("voice-pill")
                .debug_selector(|| "voice-pill".to_string())
                .occlude()
                .absolute()
                .top(px(12.0))
                .right(px(16.0))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if recording {
                    theme.danger
                } else {
                    theme.border
                })
                .bg(theme.sidebar_bg)
                .shadow_md()
                .child(
                    div()
                        .debug_selector(|| "voice-elapsed".to_string())
                        .text_color(if recording { theme.danger } else { theme.muted })
                        .child(label),
                )
                .when(recording, |d| {
                    d.child(
                        button("voice-stop", "Stop")
                            .on_click(cx.listener(|this, _e, _w, cx| this.stop_recording(cx))),
                    )
                    .child(
                        button("voice-cancel", "Cancel")
                            .on_click(cx.listener(|this, _e, _w, cx| this.cancel_recording(cx))),
                    )
                })
                .into_any_element(),
        )
    }

    /// The sidebar's "Record voice note" / "Stop recording" row.
    pub(super) fn render_voice_sidebar_item(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let recording = self.recording();
        div()
            .id("sidebar-voice")
            .debug_selector(|| "sidebar-voice".to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(recording, |d| d.text_color(theme.danger))
            .hover(|d| d.bg(theme.selected_bg))
            .on_click(cx.listener(|this, _e, _w, cx| {
                if this.recording() {
                    this.stop_recording(cx);
                } else {
                    this.start_recording(cx);
                }
            }))
            .child(if recording {
                "Stop recording"
            } else {
                "Record voice note"
            })
            .into_any_element()
    }

    /// A voice note in reading view: its name, Play, and Transcribe when a
    /// whisper program is set. `entity` stands in for `cx` (the caller
    /// holds it).
    pub(super) fn render_audio(
        &self,
        prefix: &'static str,
        ix: usize,
        n: usize,
        audio: &ImageRef,
        block: Uuid,
        entity: &Entity<NoteSec>,
    ) -> AnyElement {
        let theme = self.theme;
        let path = resolve(self.storage.root(), &audio.target).filter(|p| p.is_file());
        let name = audio
            .target
            .rsplit('/')
            .next()
            .unwrap_or(&audio.target)
            .to_string();
        let chip = div()
            .debug_selector(move || format!("{prefix}voice-{ix}-{n}"))
            .self_start()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.sidebar_bg);
        let Some(path) = path else {
            return chip
                .border_dashed()
                .text_color(theme.muted)
                .child(format!("Voice note not found: {}", audio.target))
                .into_any_element();
        };
        let button = |id: &'static str| {
            div()
                .id(id)
                .debug_selector(move || format!("{prefix}voice-{ix}-{n}-{id}"))
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .text_color(theme.accent)
                .hover(|d| d.bg(theme.selected_bg))
                // A press here never starts editing the block.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };
        let play = {
            let entity = entity.clone();
            button("play")
                .child("\u{25b6} Play")
                .on_click(move |_, _, cx| {
                    let path = path.clone();
                    entity.update(cx, |this, cx| this.play_voice_note(path, cx));
                })
        };
        let transcribe = self.whisper().is_some().then(|| {
            let entity = entity.clone();
            button("transcribe")
                .child("Transcribe")
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.transcribe_note(block, cx));
                })
        });
        chip.child(
            div()
                .text_color(theme.muted)
                .child(format!("\u{1f399} {name}")),
        )
        .child(play)
        .children(transcribe)
        .into_any_element()
    }

    /// Settings > Voice notes.
    pub(super) fn render_voice_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let button = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .cursor_pointer()
                .text_color(if active { theme.accent } else { theme.text })
                .when(active, |d| d.bg(theme.selected_bg))
                .hover(|d| d.bg(theme.selected_bg))
                .child(label)
        };
        let label = |text: &'static str| div().text_color(theme.muted).child(text);
        let path_var = self.voice_path_var();
        let (recorder, recorder_error) =
            match voice::detect(&self.config.voice_recorder, path_var.as_deref()) {
                Ok(r) => (r.describe(), false),
                Err(e) => (e, true),
            };
        let path_row = |id: &'static str, value: &str, model: bool| {
            let value = value.trim();
            let (shown, missing) = if value.is_empty() {
                ("(not set)".to_string(), false)
            } else {
                let found = std::path::Path::new(value).is_file();
                (
                    format!("{value}{}", if found { "" } else { "  (missing)" }),
                    !found,
                )
            };
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .debug_selector(move || id.to_string())
                        .min_w(px(200.0))
                        .text_color(if missing { theme.danger } else { theme.text })
                        .child(shown),
                )
                .child(
                    button(
                        if model {
                            "voice-model-choose"
                        } else {
                            "voice-whisper-choose"
                        },
                        "Choose\u{2026}",
                        false,
                    )
                    .on_click(
                        cx.listener(move |this, _e, _w, cx| this.choose_whisper_path(model, cx)),
                    ),
                )
                .when(!value.is_empty(), |d| {
                    d.child(
                        button(
                            if model {
                                "voice-model-clear"
                            } else {
                                "voice-whisper-clear"
                            },
                            "Clear",
                            false,
                        )
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.set_whisper_path(model, String::new(), cx)
                        })),
                    )
                })
        };
        let auto = self.config.voice_auto_transcribe;
        let check = self.voice.check.clone();
        div()
            .debug_selector(|| "settings-voice".to_string())
            .flex()
            .flex_col()
            .gap_2()
            .child(label("Recorder"))
            .child(
                div()
                    .debug_selector(|| "voice-recorder".to_string())
                    .text_color(if recorder_error {
                        theme.danger
                    } else {
                        theme.text
                    })
                    .child(recorder),
            )
            .child(label("Whisper program (whisper.cpp's whisper-cli)"))
            .child(path_row(
                "voice-whisper-path",
                &self.config.whisper_binary,
                false,
            ))
            .child(label("Whisper model (a ggml .bin file)"))
            .child(path_row(
                "voice-model-path",
                &self.config.whisper_model,
                true,
            ))
            .child(
                div()
                    .debug_selector(|| "voice-language".to_string())
                    .text_color(theme.muted)
                    .child(format!(
                        "Language: {} (whisper_language in config.toml)",
                        self.config.whisper_language
                    )),
            )
            .child(label("Transcribe new voice notes automatically"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(button("voice-auto-off", "Off", !auto).on_click(
                        cx.listener(|this, _e, _w, cx| this.set_auto_transcribe(false, cx)),
                    ))
                    .child(button("voice-auto-on", "On", auto).on_click(
                        cx.listener(|this, _e, _w, cx| this.set_auto_transcribe(true, cx)),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(
                        button("voice-test", "Test", false)
                            .on_click(cx.listener(|this, _e, _w, cx| this.test_whisper(cx))),
                    )
                    .children(check.map(|result| {
                        let (text, error) = match result {
                            Ok(text) => (text, false),
                            Err(text) => (text, true),
                        };
                        div()
                            .debug_selector(|| "voice-check".to_string())
                            .text_color(if error { theme.danger } else { theme.text })
                            .child(text)
                    })),
            )
            .child(div().text_color(theme.muted).child(
                "Recordings are WAV files in assets/ (about 2 MB a minute). Transcription \
                 runs the program you chose, on this computer; NoteSec sends nothing \
                 anywhere, but can't check what that program does. See docs/VOICE_NOTES.md.",
            ))
            .into_any_element()
    }
}

/// The first audio `![…](…)` in a block's text.
pub(super) fn voice_target(content: &str) -> Option<ImageRef> {
    parse_images(content)
        .into_iter()
        .find(|i| voice::is_audio_target(&i.target))
}

/// Whether block `b` has a transcript child.
fn has_transcript(blocks: &[Block], b: usize) -> bool {
    let id = blocks[b].id;
    blocks
        .iter()
        .any(|c| c.parent_id == Some(id) && c.content.starts_with(TRANSCRIPT))
}
