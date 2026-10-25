// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The captions sheet: the tray's Captions tool, as a form and then as a
//! progress report.
//!
//! One switch says what it captions: "Use Sample Voice", on, takes
//! the words from a clip with sound, chosen in the sheet from the clips on
//! the timeline that have any; off, from a script typed in it. It opens on
//! when a clip with sound is selected, or when a clip's menu opened it on
//! that clip, and off otherwise. Either lands as one batch of title clips,
//! one undo step.
//! The transcriber runs on a worker and reports as
//! [`CaptionsMsg::Progress`], ending as [`CaptionsMsg::Finished`].
//!
//! The sheet also reads a SubRip file in as captions, with the same look,
//! and writes the timeline's captions out as one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use concat_project::Command;
use concat_project::model::{ClipKind, TextStyle};
use concat_project::srt;
use concat_speech::JobStage;
use concat_speech::transcribe::Segment;

use crate::host::{on_ui_in_project, spawn_in_project};
use crate::i18n::{t, tf};
use crate::panes::Msg;
use crate::panes::pace::{Pace, Stage, Timings};
use crate::panes::settings::installed;
use crate::studio::Studio;
use crate::ui::CaptionsSheetData;

mod timing;

/// Where a caption sits, by the sheet's row: a frame-height fraction from
/// the centre, positive down. Bottom, centre, top.
const CAPTION_OFFSETS: [f64; 3] = [0.35, 0.0, -0.35];
/// A caption's cap height by the sheet's row, as a fraction of the frame.
const CAPTION_SIZES: [f64; 3] = [0.04, 0.05, 0.065];
/// A rough speaking rate, for a script's timing and the speech sheet's
/// estimate.
pub const CHARS_PER_SECOND: f32 = 14.0;

/// Everything that can happen to the captions sheet.
#[derive(Clone, Debug)]
pub enum CaptionsMsg {
    /// The tray's Captions tool: the selected clip's voice when it has
    /// sound, the script otherwise.
    Open,
    /// A clip's menu: the sheet on that clip's voice.
    OpenOnClip(String),
    Close,
    /// The "Use Sample Voice" switch.
    VoiceToggled(bool),
    /// Row in the list of clips with sound.
    SourceChanged(i32),
    TextEdited(String),
    ModelChanged(i32),
    LeadingSilenceToggled(bool),
    BackendChanged(i32),
    CloudConsentToggled(bool),
    PlacementChanged(i32),
    SizeChanged(i32),
    /// Run the pass the sheet describes.
    Begin,
    Cancel,
    /// The transcriber's worker reporting the stage it is in.
    Progress(JobStage),
    /// The transcriber's worker is done: what was said, or why not.
    Finished(Result<Vec<Segment>, String>),
    /// Asks for a SubRip file to read in.
    ImportSrt,
    /// The SubRip file chosen: its cues as captions.
    ImportSrtFrom(PathBuf),
    /// Asks where to write the captions as SubRip.
    ExportSrt,
    /// Where they go.
    ExportSrtTo(PathBuf),
}

/// The clip a transcription is running over, as the finished segments
/// need it: where it starts on the timeline and how fast it plays.
#[derive(Clone, Debug)]
struct Subject {
    start: f64,
    speed: f64,
    duration: f64,
    /// The look chosen when the run began.
    look: (f64, f64),
    /// The transcriber model, and the thirty-second windows whisper reads
    /// the sound in: what the run's timing is kept by.
    model: String,
    windows: f32,
}

/// Seconds whisper takes over one thirty-second window, per megabyte of
/// model, before a run on the model has been timed: about a second for
/// base and ten for medium on eight cores of a recent CPU.
const WINDOW_SECONDS_PER_MB: f32 = 1.0 / 160.0;

/// The captions sheet's state.
#[derive(Default)]
pub struct CaptionsPane {
    pub open: bool,
    /// "Use Sample Voice": the words come from [`Self::clip`]'s sound
    /// when on, from [`Self::text`] when off.
    pub from_voice: bool,
    /// The clip whose voice is transcribed: one with sound, chosen from the
    /// timeline's. Kept while the switch is off, so turning it back on
    /// returns to the same clip. The panel floats over a live editor, so
    /// a clip with sound selected since is the one shown and run instead;
    /// see [`Self::subject_clip`].
    pub clip: Option<String>,
    /// The sole selection when [`Self::clip`] was chosen - opened on, or
    /// picked from the list - so a later selection can be told from it.
    chosen_with: Option<String>,
    /// The script's words.
    pub text: String,
    /// Row in the installed transcriber list.
    pub model: usize,
    /// Optional energy-based prefix skipping; starts off, per session.
    pub skip_leading_silence: bool,
    /// Last provider choice, remembered in app preferences, never the project.
    pub cloud: bool,
    cloud_consent: bool,
    consent_for: Option<(concat_project::model::Clip, String)>,
    cancel: Option<Arc<AtomicBool>>,
    /// 0 bottom, 1 centre, 2 top.
    pub placement: usize,
    /// 0 small, 1 medium, 2 large.
    pub size: usize,
    pub running: bool,
    /// How far the running transcription looks.
    pub pace: Pace,
    /// Why the last run failed, when it did.
    pub message: String,
    /// The clip under the running transcription.
    subject: Option<Subject>,
    /// How long each model took this session, for the next run's bar.
    timings: Timings,
}

impl CaptionsPane {
    /// Changing provider or credentials invalidates a pending paid transfer.
    pub fn invalidate_cloud_consent(&mut self) {
        self.cloud_consent = false;
        self.consent_for = None;
    }

    /// Consent belongs to one source snapshot, never to another clip or edit.
    fn cloud_consent_valid(&self, studio: &Studio) -> bool {
        self.cloud_consent
            && self
                .subject_clip(studio, &sources(studio))
                .and_then(|id| studio.clip(&id))
                .and_then(|clip| {
                    studio.project().media_by_id(&clip.media_id).map(|media| {
                        self.consent_for.as_ref() == Some(&(clip.clone(), media.path.clone()))
                    })
                })
                .unwrap_or(false)
    }

    /// Applies one message. The studio is the rest of the window; while
    /// this runs the studio's copy of the pane is a blank it must not read.
    pub fn update(&mut self, msg: CaptionsMsg, studio: &mut Studio) {
        match msg {
            // A run in progress is shown as it is, however the panel is
            // asked for again: closing it never stopped the run.
            CaptionsMsg::Open | CaptionsMsg::OpenOnClip(_) if self.running => self.open = true,
            CaptionsMsg::Open => {
                let selected = studio.sole_selection();
                self.open_on(studio, selected, false);
            }
            CaptionsMsg::OpenOnClip(id) => {
                self.open_on(studio, Some(id), true);
            }
            CaptionsMsg::Close => {
                self.open = false;
                self.cloud_consent = false;
                self.consent_for = None;
            }
            CaptionsMsg::VoiceToggled(on) => {
                self.cloud_consent = false;
                self.consent_for = None;
                self.from_voice = on;
                self.message.clear();
                let sources = sources(studio);
                if on
                    && !self
                        .clip
                        .as_ref()
                        .is_some_and(|id| sources.iter().any(|s| &s.id == id))
                {
                    self.clip = sources.first().map(|source| source.id.clone());
                    self.chosen_with = studio.sole_selection();
                }
            }
            CaptionsMsg::SourceChanged(index) => {
                self.cloud_consent = false;
                self.consent_for = None;
                if let Some(source) = usize::try_from(index)
                    .ok()
                    .and_then(|index| sources(studio).into_iter().nth(index))
                {
                    self.clip = Some(source.id);
                    self.chosen_with = studio.sole_selection();
                    self.message.clear();
                }
            }
            CaptionsMsg::TextEdited(text) => self.text = text,
            CaptionsMsg::BackendChanged(index) => {
                if !self.running {
                    self.cloud = index == 1 && studio.prefs.transcription_cloud_enabled;
                    self.cloud_consent = false;
                    self.consent_for = None;
                    self.message.clear();
                    studio.prefs.transcription_use_cloud = self.cloud;
                    studio.prefs.save(&studio.host.dirs);
                }
            }
            CaptionsMsg::CloudConsentToggled(on) => {
                self.cloud_consent = on;
                self.consent_for = if on {
                    self.subject_clip(studio, &sources(studio))
                        .and_then(|id| studio.clip(&id))
                        .and_then(|clip| {
                            studio
                                .project()
                                .media_by_id(&clip.media_id)
                                .map(|media| (clip.clone(), media.path.clone()))
                        })
                } else {
                    None
                };
            }
            CaptionsMsg::ModelChanged(index) => self.model = index.max(0) as usize,
            CaptionsMsg::LeadingSilenceToggled(on) => self.skip_leading_silence = on,
            CaptionsMsg::PlacementChanged(index) => {
                self.placement = (index.max(0) as usize).min(2);
            }
            CaptionsMsg::SizeChanged(index) => self.size = (index.max(0) as usize).min(2),
            CaptionsMsg::Begin if self.running => {}
            CaptionsMsg::Begin => {
                if self.from_voice {
                    self.run_sound(studio);
                } else {
                    self.run_script(studio);
                }
            }
            CaptionsMsg::Cancel => {
                if let Some(cancel) = &self.cancel {
                    cancel.store(true, Ordering::Relaxed);
                }
                studio.host.transcriber.cancel();
                self.subject = None;
                self.cloud_consent = false;
                self.consent_for = None;
                self.open = false;
            }
            CaptionsMsg::Progress(stage) => self.pace.report(stage),
            CaptionsMsg::Finished(result) => {
                self.running = false;
                self.cancel = None;
                let subject = self.subject.take();
                if let (Ok(_), Some(subject)) = (&result, &subject) {
                    self.timings
                        .keep(&subject.model, self.pace.learned(subject.windows));
                }
                match result {
                    Ok(segments) => {
                        let Some(subject) = subject else {
                            return;
                        };
                        let look = subject.look;
                        let base = studio.prefs.title_style.clone();
                        // Margins are playback seconds, after accounting for clip speed.
                        let mut segments: Vec<Segment> = segments
                            .into_iter()
                            .filter(|segment| {
                                !segment.text.trim().is_empty()
                                    && segment.start.is_finite()
                                    && segment.end.is_finite()
                                    && segment.end > segment.start
                            })
                            .map(|mut segment| {
                                segment.start =
                                    (segment.start / subject.speed).clamp(0.0, subject.duration);
                                segment.end =
                                    (segment.end / subject.speed).clamp(0.0, subject.duration);
                                for token in &mut segment.tokens {
                                    token.start /= subject.speed;
                                    token.end /= subject.speed;
                                }
                                segment
                            })
                            .filter(|segment| segment.end > segment.start)
                            .collect();
                        segments.sort_by(|a, b| a.start.total_cmp(&b.start));
                        let ranges: Vec<(f64, f64)> = segments
                            .iter()
                            .map(|segment| (segment.start, segment.end))
                            .collect();
                        let ranges = timing::readable_ranges(&ranges, subject.duration);
                        let commands: Vec<Command> = segments
                            .into_iter()
                            .zip(ranges)
                            .filter_map(|(segment, (start, end))| {
                                let text = segment.text.trim().to_owned();
                                // Respect the one-frame floor without overlapping neighbours.
                                let duration = end - start;
                                if duration < concat_project::model::ranges::MIN_CLIP_DURATION {
                                    return None;
                                }
                                let word_starts = caption_word_starts(&segment, start, duration);
                                if word_starts.is_empty() {
                                    log::warn!("caption at {:.2}s has no usable Whisper word timing; using regular cadence", segment.start);
                                }
                                Some(caption_clip(
                                    text,
                                    subject.start + start,
                                    duration,
                                    word_starts,
                                    look,
                                    base.as_ref(),
                                ))
                            })
                            .collect();
                        let count = commands.len();
                        self.open = false;
                        if count == 0 {
                            studio.notify(&t("captions.nothingSaidClip"), true);
                        } else {
                            studio.apply(Command::Batch { commands });
                            studio.notify(&tf("captions.addedCaptions", &[&count]), false);
                        }
                    }
                    // Asked for: the sheet is already on its way down.
                    Err(error) if error.contains("cancel") => self.open = false,
                    // Closed while it ran: the panel is not there to say
                    // it, so the notice does, and the panel too next time.
                    Err(error) => {
                        if !self.open {
                            studio.notify(&error, true);
                        }
                        self.message = error;
                    }
                }
            }
            // Asked with the studio unborrowed, on a turn of its own; see
            // `host::after_dialog`.
            CaptionsMsg::ImportSrt => crate::host::pick_files_then(
                t("captions.importSrt"),
                Some((t("captions.subRipSubtitles"), &["srt"])),
                |studio, _, _, paths| {
                    if let Some(path) = paths.into_iter().next() {
                        studio.handle(Msg::Captions(CaptionsMsg::ImportSrtFrom(path)));
                    }
                },
            ),
            CaptionsMsg::ImportSrtFrom(path) => self.import_srt(studio, &path),
            CaptionsMsg::ExportSrt => self.ask_export_srt(studio),
            CaptionsMsg::ExportSrtTo(path) => self.export_srt(studio, &path),
        }
    }

    /// A SubRip file's cues as captions where the file times them, with
    /// the sheet's look: one batch, one undo step. A file that is not
    /// UTF-8 is read as best it can be.
    fn import_srt(&mut self, studio: &mut Studio, path: &Path) {
        let cues = std::fs::read(path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                srt::parse(&String::from_utf8_lossy(&bytes)).map_err(|error| error.to_string())
            });
        let cues = match cues {
            Ok(cues) => cues,
            Err(error) => {
                log::warn!("captions: could not read {}: {error}", path.display());
                self.message = tf("captions.couldNotReadSubtitles", &[&error]);
                return;
            }
        };
        let look = self.look();
        let base = studio.prefs.title_style.clone();
        let commands = srt_commands(cues, look, base.as_ref());
        if commands.is_empty() {
            self.message = t("captions.noSubtitlesInFile");
            return;
        }
        let count = commands.len();
        self.open = false;
        studio.apply(Command::Batch { commands });
        studio.notify(&tf("captions.addedCaptions", &[&count]), false);
    }

    /// Asks where to write the captions, offering the project's folder
    /// and its name. A phone has no such dialog: the file is written
    /// beside the project under a name nothing there has, and handed to
    /// the phone to put where its file manager shows it.
    fn ask_export_srt(&mut self, studio: &mut Studio) {
        if export_lane(studio).is_none() {
            self.message = t("captions.noCaptionsToExport");
            return;
        }
        let name = format!("{}.srt", file_stem(studio));
        // No project, no project folder: Documents, as anywhere else a
        // file is written with nothing better to offer.
        let folder = studio
            .session
            .as_ref()
            .map(|session| PathBuf::from(session.path()))
            .unwrap_or_else(|| PathBuf::from(crate::studio::home_folder("Documents")));
        if cfg!(any(target_os = "android", target_os = "ios")) {
            let path = unique_path(&folder, &name);
            self.export_srt(studio, &path);
            return;
        }
        let title = t("captions.exportSrt");
        let family = t("captions.subRipSubtitles");
        crate::host::after_dialog(
            move || {
                let chosen = crate::platform::save_file(
                    &title,
                    &folder,
                    &name,
                    (family.as_str(), &["srt"]),
                )?;
                Some(
                    if chosen
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("srt"))
                    {
                        chosen
                    } else {
                        chosen.with_extension("srt")
                    },
                )
            },
            |studio, _, _, path| studio.handle(Msg::Captions(CaptionsMsg::ExportSrtTo(path))),
        );
    }

    /// Writes the captions to `path` as SubRip, and says how many and
    /// where - on a phone, once the phone has put the file where it shows
    /// it.
    fn export_srt(&mut self, studio: &mut Studio, path: &Path) {
        let Some(lane) = export_lane(studio) else {
            self.message = t("captions.noCaptionsToExport");
            return;
        };
        let cues = srt::title_cues(studio.timeline(), Some(&lane.track_id));
        if let Err(error) = std::fs::write(path, srt::write(&cues)) {
            log::warn!("captions: could not write {}: {error}", path.display());
            self.message = tf("captions.couldNotWriteSubtitles", &[&error]);
            return;
        }
        log::info!(
            "captions: wrote {} cue(s) to {}",
            cues.len(),
            path.display()
        );
        self.open = false;
        let count = cues.len();
        // Which lane was written goes without saying when it was the
        // selected title's, or the only lane with titles; otherwise the
        // notice says it was the busiest, and how to pick another.
        let busiest = lane.busiest && srt::title_lanes(studio.timeline()) > 1;
        let say = move |studio: &mut Studio, place: &str| {
            // Each key spelled out where it is used: scripts/locales.py
            // finds the strings the source asks for by reading it.
            let said = if busiest {
                tf("captions.exportedBusiestLane", &[&count, &place])
            } else {
                tf("captions.exportedCaptions", &[&count, &place])
            };
            studio.notify(&said, false);
        };
        let written = path.to_path_buf();
        let handed = crate::platform::publish_document(
            path.to_path_buf(),
            "application/x-subrip",
            move |result| {
                crate::host::on_ui(move |studio, _, _| match result {
                    Ok(place) => say(studio, &place),
                    Err(error) => {
                        log::warn!("captions: could not publish {}: {error}", written.display());
                        studio.notify(
                            &tf(
                                "captions.couldNotPublishSubtitles",
                                &[&written.display(), &error],
                            ),
                            true,
                        );
                    }
                });
            },
        );
        if !handed {
            say(studio, &path.display().to_string());
        }
    }

    /// The sheet opened afresh. `clip` is the clip it was opened on, if
    /// any: the switch starts on when that clip has sound, or when a clip's
    /// menu asked for its voice (`voice`), in which case a clip without
    /// sound falls to the first clip that has some.
    fn open_on(&mut self, studio: &Studio, clip: Option<String>, voice: bool) {
        let installed = installed(&studio.settings.transcribers);
        let model = installed.iter().position(|model| model.active).unwrap_or(0);
        let sources = sources(studio);
        let sounding = clip.filter(|id| sources.iter().any(|source| &source.id == id));
        let from_voice = voice || sounding.is_some();
        let clip = sounding.or_else(|| {
            from_voice
                .then(|| sources.first().map(|source| source.id.clone()))
                .flatten()
        });
        *self = CaptionsPane {
            open: true,
            cloud: studio.prefs.transcription_cloud_enabled && studio.prefs.transcription_use_cloud,
            from_voice,
            clip,
            chosen_with: studio.sole_selection(),
            model,
            placement: 0,
            size: 1,
            timings: std::mem::take(&mut self.timings),
            ..CaptionsPane::default()
        };
    }

    /// The clip the panel transcribes, and the one its list shows - the
    /// same answer for both, so it never runs a clip it is not showing.
    /// The editor stays live under the panel: a clip with sound selected
    /// since the panel's clip was chosen is the one meant now, and is
    /// taken. Otherwise the chosen clip while it is still on the timeline
    /// with sound, and none - nothing to run - when it is not.
    fn subject_clip(&self, studio: &Studio, sources: &[Source]) -> Option<String> {
        follow_selection(
            studio.sole_selection(),
            self.chosen_with.as_deref(),
            self.clip.as_deref(),
            |id| sources.iter().any(|source| source.id == id),
        )
    }

    /// A caption's look, by the sheet's rows: where it sits and its size.
    fn look(&self) -> (f64, f64) {
        (
            CAPTION_OFFSETS[self.placement.min(2)],
            CAPTION_SIZES[self.size.min(2)],
        )
    }

    /// The script as titles, one after another from the playhead.
    fn run_script(&mut self, studio: &mut Studio) {
        let lines = script_captions(&self.text);
        if lines.is_empty() {
            self.message = t("captions.nothingToCaptionYet");
            return;
        }
        let look = self.look();
        let mut at = f64::from(studio.playhead);
        let base = studio.prefs.title_style.clone();
        let commands: Vec<Command> = lines
            .into_iter()
            .map(|(text, seconds)| {
                let command = caption_clip(text, at, seconds, Vec::new(), look, base.as_ref());
                at += seconds;
                command
            })
            .collect();
        let count = commands.len();
        self.open = false;
        studio.apply(Command::Batch { commands });
        studio.notify(&tf("captions.addedCaptions", &[&count]), false);
    }

    /// The sheet's clip through the transcriber on a worker, reporting into
    /// the sheet as it goes.
    fn run_sound(&mut self, studio: &mut Studio) {
        // The clip the panel shows, and no other: see `subject_clip`.
        let Some(clip) = self
            .subject_clip(studio, &sources(studio))
            .and_then(|id| studio.clip(&id))
            .cloned()
        else {
            self.message = t("captions.clipNoLongerTimeline");
            return;
        };
        let Some(media) = studio.project().media_by_id(&clip.media_id).cloned() else {
            self.message = t("captions.clipHasNoFile");
            return;
        };
        let cloud = self.cloud && studio.prefs.transcription_cloud_enabled;
        let (model, megabytes) = if cloud {
            if !self.cloud_consent
                || self.consent_for.as_ref() != Some(&(clip.clone(), media.path.clone()))
            {
                self.cloud_consent = false;
                self.consent_for = None;
                self.message = t("captions.cloudConsentRequired");
                return;
            }
            if !studio.prefs.transcription_cloud_enabled || !studio.settings.cloud_ready() {
                self.message = t("captions.cloudCredential");
                return;
            }
            (concat_speech::cloud::MODEL.to_owned(), 0.0)
        } else if let Some(model) = installed(&studio.settings.transcribers).get(self.model) {
            (model.id.clone(), model.megabytes)
        } else {
            self.message = t("captions.downloadTranscriberModelFirst");
            return;
        };
        let request = concat_speech::transcribe::TranscribeRequest {
            backend: if cloud {
                concat_speech::transcribe::Backend::OpenRouter
            } else {
                concat_speech::transcribe::Backend::Whisper
            },
            cloud_consent: self.cloud_consent,
            path: media.path.clone(),
            audio_stream: clip.audio_stream,
            source_start: clip.source_start,
            window: clip.duration * clip.speed,
            model_id: model.clone(),
            skip_leading_silence: self.skip_leading_silence,
        };
        self.cloud_consent = false;
        self.consent_for = None;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(Arc::clone(&cancel));
        let dirs = studio.host.dirs.clone();
        let transcriber = Arc::clone(&studio.host.transcriber);
        // The run as the bar lays it out: the sound read in at a few
        // hundred times its length, the model loaded unless it still is,
        // and whisper a window at a time - at the pace the last run on
        // this model measured, or the estimate by its size.
        let windows = (request.window / 30.0).ceil().max(1.0) as f32;
        let learned = self.timings.get(&model);
        let loaded = studio.host.transcriber.loaded_model().as_deref() == Some(model.as_str());
        let load = if loaded {
            0.0
        } else {
            learned.load.unwrap_or(0.5 + megabytes / 400.0)
        };
        let per_window = learned
            .per_unit
            .unwrap_or(0.3 + megabytes * WINDOW_SECONDS_PER_MB);
        self.pace = Pace::new(vec![
            (Stage::Reading, 0.2 + request.window as f32 / 400.0),
            (Stage::Loading, load),
            (Stage::Working, windows * per_window),
        ]);
        if cloud {
            self.pace = Pace::new(vec![(Stage::Reading, 1.0), (Stage::Working, 60.0)]);
        }
        self.subject = Some(Subject {
            start: clip.start,
            speed: clip.speed,
            duration: clip.duration,
            look: self.look(),
            model,
            windows,
        });
        self.running = true;
        self.message.clear();
        let epoch = crate::host::project_epoch();
        spawn_in_project(
            move || {
                transcriber.transcribe_cancellable(&dirs, &request, cancel, move |stage| {
                    on_ui_in_project(epoch, move |studio, _, _| {
                        studio.handle(Msg::Captions(CaptionsMsg::Progress(stage)));
                    });
                })
            },
            |studio, _, _, result| studio.handle(Msg::Captions(CaptionsMsg::Finished(result))),
        );
    }

    /// The sheet as Slint shows it.
    pub fn data(&self, studio: &Studio) -> CaptionsSheetData {
        let sources = if self.open {
            sources(studio)
        } else {
            Vec::new()
        };
        let source = self
            .subject_clip(studio, &sources)
            .and_then(|id| sources.iter().position(|source| source.id == id))
            .map_or(-1, |index| index as i32);
        let labels: Vec<slint::SharedString> = sources
            .into_iter()
            .map(|source| source.label.into())
            .collect();
        CaptionsSheetData {
            open: self.open,
            from_voice: self.from_voice,
            sources: std::rc::Rc::new(slint::VecModel::from(labels)).into(),
            source,
            text: self.text.as_str().into(),
            model: self.model as i32,
            skip_leading_silence: self.skip_leading_silence,
            backend: i32::from(self.cloud && studio.prefs.transcription_cloud_enabled),
            cloud_enabled: studio.prefs.transcription_cloud_enabled,
            cloud_configured: studio.settings.cloud_ready(),
            cloud_consent: self.cloud_consent,
            placement: self.placement as i32,
            size: self.size as i32,
            running: self.running,
            progress: self.pace.floor,
            ceiling: self.pace.ceiling,
            step: self.pace.step,
            left: self.pace.left,
            stage: match self.pace.stage() {
                Some(Stage::Loading) => t("captions.loadingTheModel"),
                Some(Stage::Working) => t("captions.transcribing"),
                _ => t("captions.readingTheSound"),
            }
            .into(),
            ready: if self.cloud && studio.prefs.transcription_cloud_enabled {
                self.cloud_consent_valid(studio) && studio.settings.cloud_ready()
            } else {
                !installed(&studio.settings.transcribers).is_empty()
            },
            message: self.message.as_str().into(),
        }
    }
}

/// Imported cues keep the file's timing, including captions shorter than
/// generated speech segments. The title command applies the engine's minimum
/// clip duration where needed.
fn srt_commands(cues: Vec<srt::Cue>, look: (f64, f64), base: Option<&TextStyle>) -> Vec<Command> {
    cues.into_iter()
        .filter(|cue| !cue.text.trim().is_empty())
        .map(|cue| {
            caption_clip(
                cue.text.trim().to_owned(),
                cue.start,
                cue.end - cue.start,
                Vec::new(),
                look,
                base,
            )
        })
        .collect()
}

/// The lane an SRT export writes out.
struct ExportLane {
    track_id: String,
    /// Chosen as the lane with the most titles, not by a selected title.
    busiest: bool,
}

/// The lane whose titles are written out as captions: the selected
/// title's; or with no title selected, the lane with the most titles -
/// captions come many to a lane, a title card one or two, and nothing
/// else in the document tells them apart (see `srt::busiest_title_lane`).
/// None when the timeline has no title.
fn export_lane(studio: &Studio) -> Option<ExportLane> {
    let selected = studio
        .sole_selection()
        .and_then(|id| studio.clip(&id))
        .filter(|clip| clip.kind == ClipKind::Text)
        .map(|clip| clip.track_id.clone());
    match selected {
        Some(track_id) => Some(ExportLane {
            track_id,
            busiest: false,
        }),
        None => srt::busiest_title_lane(studio.timeline()).map(|track_id| ExportLane {
            track_id,
            busiest: true,
        }),
    }
}

/// The name an export is offered under: the project's, as one file name,
/// or "captions" with no project or no usable name.
fn file_stem(studio: &Studio) -> String {
    let name = studio
        .session
        .as_ref()
        .map(|session| session.settings().name)
        .unwrap_or_default();
    let stem: String = name
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stem = stem.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if stem.is_empty() {
        "captions".to_owned()
    } else {
        stem.to_owned()
    }
}

/// `name` in `dir`, or `name (2)`, `name (3)`... until nothing there is
/// written over.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

/// [`CaptionsPane::subject_clip`] with the studio taken out: `selected`
/// the sole selection, `chosen_with` the selection when `clip` was chosen,
/// and `sounds` whether a clip is on the timeline with sound.
fn follow_selection(
    selected: Option<String>,
    chosen_with: Option<&str>,
    clip: Option<&str>,
    sounds: impl Fn(&str) -> bool,
) -> Option<String> {
    if let Some(selected) = selected.filter(|id| sounds(id))
        && Some(selected.as_str()) != chosen_with
    {
        return Some(selected);
    }
    clip.filter(|id| sounds(id)).map(str::to_owned)
}

/// A clip on the timeline whose voice can be captioned, as the sheet's
/// list shows it.
struct Source {
    id: String,
    label: String,
}

/// The clips on the current timeline that have sound, in the order they
/// play: an audio clip, or a video whose file has an audio stream.
fn sources(studio: &Studio) -> Vec<Source> {
    let project = studio.project();
    let mut clips: Vec<_> = studio
        .timeline()
        .clips
        .iter()
        .filter(|clip| studio.clip_has_sound(clip))
        .map(|clip| {
            let name = project
                .media_by_id(&clip.media_id)
                .map_or(clip.name.as_str(), |media| media.name.as_str());
            (clip.start, clip.id.clone(), name.to_owned())
        })
        .collect();
    clips.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    clips
        .into_iter()
        .map(|(start, id, name)| Source {
            id,
            label: source_label(&name, start),
        })
        .collect()
}

/// A clip as the list names it: its file and where it starts on the
/// timeline, so two clips of one file are told apart.
fn source_label(name: &str, start: f64) -> String {
    let seconds = start.max(0.0).floor() as u64;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{name} · {hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{name} · {minutes}:{seconds:02}")
    }
}

/// Use the first text-bearing token of each renderer word. Multiple tokens
/// may spell one word, or one token may contain several words (same onset).
/// Neither case needs invented interpolation. Reject incomplete/nonmonotonic
/// estimates as a whole, allowing the regular cadence the user requested.
fn word_starts_from_segment(segment: &Segment, duration: f64, speed: f64) -> Vec<f64> {
    if !duration.is_finite()
        || duration <= 0.0
        || !speed.is_finite()
        || speed <= 0.0
        || !segment.start.is_finite()
        || !segment.end.is_finite()
        || segment.end <= segment.start
    {
        return Vec::new();
    }
    let mut starts = Vec::new();
    for word in concat_text::word_ranges(&segment.text) {
        let Some(token) = segment
            .tokens
            .iter()
            .find(|token| token.byte_start <= word.start && word.start < token.byte_end)
            .or_else(|| {
                segment
                    .tokens
                    .iter()
                    .find(|token| token.byte_start < word.end && word.start < token.byte_end)
            })
        else {
            return Vec::new();
        };
        if token.byte_end > segment.text.len()
            || !token.start.is_finite()
            || !token.end.is_finite()
            || token.end < token.start
            || token.start < segment.start - 1e-6
            || token.start >= segment.end
        {
            return Vec::new();
        }
        let at = ((token.start - segment.start) / speed / duration).clamp(0.0, 1.0);
        if starts.last().is_some_and(|previous| *previous > at) {
            return Vec::new();
        }
        starts.push(at);
    }
    // A whole sentence with identical word onsets must use regular cadence,
    // rather than appearing at once. Keep partial ties when other words advance.
    if starts.len() > 1 && !starts.windows(2).any(|pair| pair[1] > pair[0] + 1e-9) {
        return Vec::new();
    }
    starts
}

/// Speech and token times here are already playback seconds. Preserve their
/// acoustic clock while normalizing against the final padded caption bounds.
fn caption_word_starts(segment: &Segment, caption_start: f64, duration: f64) -> Vec<f64> {
    if !caption_start.is_finite() || !duration.is_finite() || duration <= 0.0 {
        return Vec::new();
    }
    let speech_duration = segment.end - segment.start;
    let starts = word_starts_from_segment(segment, speech_duration, 1.0);
    let mut out = Vec::with_capacity(starts.len());
    for onset in starts {
        let seconds = segment.start + onset * speech_duration;
        // A word outside a shortened caption cannot be shown in sync.
        if seconds >= caption_start + duration {
            return Vec::new();
        }
        out.push(((seconds - caption_start) / duration).clamp(0.0, 1.0));
    }
    out
}

/// One caption as a title clip: its words, when and for how long, and
/// its look.
fn caption_clip(
    text: String,
    start: f64,
    duration: f64,
    word_starts: Vec<f64>,
    look: (f64, f64),
    base: Option<&TextStyle>,
) -> Command {
    let (offset_y, font_size) = look;
    // The kept look where there is one - family, weight, colours, stroke,
    // shadow, background - at the caption's own size and place; the
    // bundled face otherwise.
    let style = match base {
        Some(base) => TextStyle {
            word_starts,
            content: text,
            font_size,
            max_width: 0.0,
            max_height: 0.0,
            // Runs are some words' looks, and these are other words.
            runs: Vec::new(),
            ..base.clone()
        },
        None => TextStyle {
            word_starts,
            content: text,
            font_family: "Hanken Grotesk".to_owned(),
            font_size,
            font_weight: 600.0,
            ..TextStyle::default()
        },
    };
    Command::AddTextClip {
        track_id: None,
        above: true,
        start,
        style: Some(style),
        duration: Some(duration),
        offset_y: Some(offset_y),
    }
}

/// Longest a caption line gets before it is wrapped: about what two lines
/// of broadcast subtitle hold, and what a reader takes in at a glance.
const CAPTION_CHARS: usize = 42;

/// A script as caption lines, each with how long it stays up: a line's
/// reading time at [`CHARS_PER_SECOND`], held to one second at least so
/// a short word is not a flicker, and seven at most so a long line does
/// not hang. A line break in the script is a break the author asked for;
/// within a paragraph a sentence is a caption, and a long sentence wraps
/// at its words.
fn script_captions(text: &str) -> Vec<(String, f64)> {
    text.lines()
        .flat_map(sentences)
        .flat_map(|sentence| wrap_caption(&sentence))
        .map(|line| {
            let seconds =
                (line.chars().count() as f64 / f64::from(CHARS_PER_SECOND)).clamp(1.0, 7.0);
            (line, seconds)
        })
        .collect()
}

/// A paragraph's sentences. A full stop, question or exclamation mark ends
/// one when it is followed by space or by the end - so "3.5" and "e.g." hold
/// together - and the CJK marks end one on their own.
fn sentences(paragraph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = paragraph.chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        let ends = match ch {
            '。' | '！' | '？' => true,
            '.' | '!' | '?' => chars.peek().is_none_or(|next| next.is_whitespace()),
            _ => false,
        };
        if ends {
            let sentence = current.trim();
            if !sentence.is_empty() {
                out.push(sentence.to_owned());
            }
            current.clear();
        }
    }
    let rest = current.trim();
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    out
}

/// A sentence in lines of at most [`CAPTION_CHARS`], broken between words;
/// a word longer than a line, or a run of CJK with no spaces, is broken
/// where it must be.
fn wrap_caption(sentence: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_chars = 0;
    for word in sentence.split_whitespace() {
        let word_chars = word.chars().count();
        if line_chars > 0 && line_chars + 1 + word_chars > CAPTION_CHARS {
            lines.push(std::mem::take(&mut line));
            line_chars = 0;
        }
        if word_chars > CAPTION_CHARS {
            let mut piece = String::new();
            for ch in word.chars() {
                piece.push(ch);
                if piece.chars().count() == CAPTION_CHARS {
                    lines.push(std::mem::take(&mut piece));
                }
            }
            line = piece;
            line_chars = line.chars().count();
            continue;
        }
        if line_chars > 0 {
            line.push(' ');
            line_chars += 1;
        }
        line.push_str(word);
        line_chars += word_chars;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{caption_clip, script_captions, source_label, word_starts_from_segment};
    use concat_project::{Command, model::TextStyle};
    use concat_speech::transcribe::{Segment, TokenTiming};

    #[test]
    fn cloud_words_match_renderer_ranges_with_unmeasured_punctuation() {
        let segment = Segment {
            start: 0.0,
            end: 2.0,
            text: "\"Ciao\" mondo!".into(),
            tokens: vec![
                TokenTiming {
                    byte_start: 1,
                    byte_end: 5,
                    start: 0.2,
                    end: 0.7,
                },
                TokenTiming {
                    byte_start: 7,
                    byte_end: 13,
                    start: 1.0,
                    end: 1.5,
                },
            ],
        };
        assert_eq!(word_starts_from_segment(&segment, 2.0, 1.0), vec![0.1, 0.5]);
    }

    #[test]
    fn padding_does_not_advance_or_stretch_measured_word_times() {
        let segment = timed_caption();
        let starts = super::caption_word_starts(&segment, 0.25, 3.5);
        assert_eq!(starts.len(), 2);
        assert!((0.25 + starts[0] * 3.5 - 1.1).abs() < 1e-9);
        assert!((0.25 + starts[1] * 3.5 - 1.9).abs() < 1e-9);
    }

    #[test]
    fn restored_offset_speed_and_padding_use_one_clock() {
        let mut segment = timed_caption();
        segment.start = (segment.start + 4.01) / 2.0;
        segment.end = (segment.end + 4.01) / 2.0;
        for token in &mut segment.tokens {
            token.start = (token.start + 4.01) / 2.0;
            token.end = (token.end + 4.01) / 2.0;
        }
        let caption_start = segment.start - 0.75;
        let starts = super::caption_word_starts(&segment, caption_start, 2.5);
        assert_eq!(starts.len(), 2);
        assert!((caption_start + starts[0] * 2.5 - (1.1 + 4.01) / 2.0).abs() < 1e-9);
        assert!((caption_start + starts[1] * 2.5 - (1.9 + 4.01) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn caption_cut_before_a_word_uses_regular_fallback() {
        assert!(super::caption_word_starts(&timed_caption(), 1.0, 0.5).is_empty());
    }

    fn timed_caption() -> Segment {
        let pieces = ["C", "'", "era", " cos", "ì", "."];
        let times = [1.1, 1.12, 1.2, 1.9, 2.0, 2.2];
        let mut cursor = 0;
        let tokens = pieces
            .iter()
            .zip(times)
            .map(|(text, start)| {
                let token = TokenTiming {
                    byte_start: cursor,
                    byte_end: cursor + text.len(),
                    start,
                    end: start + 0.05,
                };
                cursor += text.len();
                token
            })
            .collect();
        Segment {
            start: 1.0,
            end: 3.0,
            text: pieces.concat(),
            tokens,
        }
    }

    #[test]
    fn whisper_subwords_become_word_onsets_not_regular_steps() {
        let segment = timed_caption();
        let starts = word_starts_from_segment(&segment, 2.0, 1.0);
        assert_eq!(starts.len(), 2, "apostrophe and accents stay in their word");
        assert!((starts[0] - 0.05).abs() < 1e-9);
        assert!((starts[1] - 0.45).abs() < 1e-9);
        assert_eq!(word_starts_from_segment(&segment, 1.0, 2.0), starts);
        // A minimum caption duration must not stretch acoustic onsets.
        let stretched = word_starts_from_segment(&segment, 4.0, 1.0);
        assert!((stretched[1] - starts[1] / 2.0).abs() < 1e-9);
        let look = (0.35, 0.05);
        let base = TextStyle {
            word_reveal: true,
            ..Default::default()
        };
        let command = caption_clip(segment.text, 10.0, 2.0, starts.clone(), look, Some(&base));
        match command {
            Command::AddTextClip {
                style: Some(style),
                start,
                duration: Some(duration),
                ..
            } => {
                assert_eq!(start, 10.0);
                assert_eq!(duration, 2.0);
                assert_eq!(style.word_starts, starts);
                assert!(style.word_reveal);
            }
            _ => panic!("one caption containing its word timing, no batch or extra clips"),
        }
    }

    #[test]
    fn bad_or_missing_word_timing_uses_the_regular_fallback() {
        let mut segment = timed_caption();
        segment.tokens.clear();
        assert!(word_starts_from_segment(&segment, 2.0, 1.0).is_empty());
        let fallback = TextStyle::default().word_onsets(2);
        assert_eq!(fallback, vec![0.0, 0.8]);
        let mut segment = timed_caption();
        segment.tokens[0].start = -1.0;
        assert!(word_starts_from_segment(&segment, 2.0, 1.0).is_empty());
        let mut segment = timed_caption();
        segment.tokens[3].start = 1.01;
        assert!(word_starts_from_segment(&segment, 2.0, 1.0).is_empty());
        assert!(word_starts_from_segment(&segment, 2.0, 0.0).is_empty());
    }

    #[test]
    fn identical_onsets_for_every_word_use_regular_fallback() {
        let segment = Segment {
            start: 4.0,
            end: 8.0,
            text: "one two".into(),
            tokens: vec![TokenTiming {
                byte_start: 0,
                byte_end: 7,
                start: 5.0,
                end: 7.0,
            }],
        };
        assert!(word_starts_from_segment(&segment, 4.0, 1.0).is_empty());
        let mut segment = timed_caption();
        for token in &mut segment.tokens {
            token.start = 1.0;
            token.end = 1.0;
        }
        assert!(word_starts_from_segment(&segment, 2.0, 1.0).is_empty());
    }

    #[test]
    fn partial_shared_onsets_are_kept_when_the_sentence_advances() {
        let segment = Segment {
            start: 4.0,
            end: 8.0,
            text: "one two three".into(),
            tokens: vec![
                TokenTiming {
                    byte_start: 0,
                    byte_end: 7,
                    start: 5.0,
                    end: 6.0,
                },
                TokenTiming {
                    byte_start: 7,
                    byte_end: 13,
                    start: 7.0,
                    end: 8.0,
                },
            ],
        };
        assert_eq!(
            word_starts_from_segment(&segment, 4.0, 1.0),
            vec![0.25, 0.25, 0.75]
        );
    }

    #[test]
    fn imported_srt_preserves_short_cues_and_multiline_text() {
        let cues = concat_project::srt::parse(
            "1\n00:00:01,000 --> 00:00:01,100\nOlá\nAção\n\n2\n00:00:02,000 --> 00:00:03,000\nFim\n",
        ).unwrap();
        let commands = super::srt_commands(cues.clone(), (0.35, 0.04), None);
        let mut editor = concat_project::Editor::new();
        editor
            .apply(concat_project::Command::Batch { commands })
            .unwrap();
        let exported = concat_project::srt::write(&concat_project::srt::title_cues(
            editor.project().active(),
            None,
        ));
        assert_eq!(concat_project::srt::parse(&exported).unwrap(), cues);
        assert!(editor.undo());
        assert!(editor.project().active().clips.is_empty());
        assert!(editor.redo());
        assert_eq!(
            concat_project::srt::title_cues(editor.project().active(), None).len(),
            2
        );
    }

    /// The panel floats over a live editor: a clip with sound selected
    /// since the panel's clip was chosen is the one it shows and runs; a
    /// pick from its list holds until the selection moves again; and a
    /// chosen clip gone from the timeline is not run unseen.
    #[test]
    fn the_captioned_clip_follows_a_new_selection_of_a_clip_with_sound() {
        let sounds = |id: &str| ["a", "b", "c"].contains(&id);
        let follow = |selected: Option<&str>, chosen_with: Option<&str>, clip: Option<&str>| {
            super::follow_selection(selected.map(str::to_owned), chosen_with, clip, sounds)
        };
        // Opened on A, A still selected: A.
        assert_eq!(
            follow(Some("a"), Some("a"), Some("a")).as_deref(),
            Some("a")
        );
        // B selected on the timeline since: B.
        assert_eq!(
            follow(Some("b"), Some("a"), Some("a")).as_deref(),
            Some("b")
        );
        // C picked from the list with B selected: C, until the selection moves.
        assert_eq!(
            follow(Some("b"), Some("b"), Some("c")).as_deref(),
            Some("c")
        );
        // A title selected - no sound - leaves the choice alone.
        assert_eq!(
            follow(Some("t"), Some("b"), Some("c")).as_deref(),
            Some("c")
        );
        // The chosen clip deleted: nothing to run, rather than a clip unseen.
        assert_eq!(follow(None, None, Some("gone")), None);
    }

    #[test]
    fn imported_srt_ignores_empty_captions() {
        let cues = vec![concat_project::srt::Cue {
            start: 0.0,
            end: 1.0,
            text: "  ".into(),
        }];
        assert!(super::srt_commands(cues, (0.35, 0.04), None).is_empty());
    }

    /// A clip in the voice list is its file and where it starts, in
    /// minutes and seconds, with hours once there are any.
    #[test]
    fn a_voice_is_named_by_its_file_and_where_it_starts() {
        assert_eq!(source_label("Interview.mp4", 0.0), "Interview.mp4 · 0:00");
        assert_eq!(source_label("Interview.mp4", 72.9), "Interview.mp4 · 1:12");
        assert_eq!(source_label("Talk.wav", 3_725.0), "Talk.wav · 1:02:05");
        assert_eq!(source_label("Talk.wav", -1.0), "Talk.wav · 0:00");
    }

    /// A script becomes one caption per sentence, a hand line break is
    /// kept, a long sentence wraps at its words, and each line is held for
    /// its reading time within one to seven seconds.
    #[test]
    fn a_script_is_cut_into_readable_lines() {
        let lines = script_captions(
            "Hello there. This is version 3.5, mind!\n\nA sentence that runs on for far \
             longer than a caption line has any business running on for. Ok?",
        );
        let text: Vec<&str> = lines.iter().map(|(line, _)| line.as_str()).collect();
        assert_eq!(
            text,
            [
                "Hello there.",
                "This is version 3.5, mind!",
                "A sentence that runs on for far longer",
                "than a caption line has any business",
                "running on for.",
                "Ok?",
            ]
        );
        assert!(
            lines
                .iter()
                .all(|(_, seconds)| (1.0..=7.0).contains(seconds))
        );
        assert_eq!(lines[0].1, 1.0);
        assert!(lines[2].1 > lines[0].1);
        assert!(script_captions("  \n ").is_empty());
        assert_eq!(script_captions("你好。再见！").len(), 2);
    }
}
