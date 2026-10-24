// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! One replaceable preview request, rendered away from the UI and its font lock.

use super::*;

/// Progress of the current preview's word images, never the export's readiness.
#[derive(Clone, Debug, Default)]
pub struct PreparationStatus {
    /// Changes when a different set of styles or dimensions is requested.
    pub generation: u64,
    /// The requested preview is still being prepared.
    pub preparing: bool,
    /// Completed word images, including ones reused from disk.
    pub completed: usize,
    /// Word images required for the missing styles.
    pub total: usize,
    /// Preparation failed; the preview remains readable as static text.
    pub failed: bool,
}

#[derive(Default)]
pub(super) struct Preparation {
    signature: Vec<u64>,
    pending: Option<Request>,
    running: bool,
    status: PreparationStatus,
}

struct Request {
    generation: u64,
    project: Project,
    width: u32,
    height: u32,
    styles: Vec<TextStyle>,
}

impl Titles {
    /// Preparation is polled by the window; reading this never waits for rendering.
    pub fn preparation_status(&self) -> PreparationStatus {
        self.preparation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }

    /// Cancel an obsolete preview, for example when its project closes.
    pub fn cancel_preparation(&self) {
        let mut state = self.preparation.lock().unwrap_or_else(|e| e.into_inner());
        state.signature.clear();
        state.pending = None;
        state.status = PreparationStatus {
            generation: state.status.generation + 1,
            ..PreparationStatus::default()
        };
    }

    pub(super) fn request_words(&self, project: &Project, width: u32, height: u32) {
        let mut seen = HashSet::new();
        let mut signature = Vec::new();
        let mut missing = Vec::new();
        for clip in &project.active().clips {
            if clip.kind != ModelClipKind::Text {
                continue;
            }
            let Some(style) = clip.text.as_ref().filter(|style| style.word_reveal) else {
                continue;
            };
            let key = key_of(project, Painting::Title(style), width, height);
            if !seen.insert(key) {
                continue;
            }
            signature.push(key);
            if self.cached_words(key).is_none() {
                missing.push(style.clone());
            }
        }
        signature.sort_unstable();
        let mut state = self.preparation.lock().unwrap_or_else(|e| e.into_inner());
        if state.signature == signature {
            return;
        }
        state.signature = signature;
        let generation = state.status.generation + 1;
        state.status = PreparationStatus {
            generation,
            preparing: !missing.is_empty(),
            total: missing
                .iter()
                .map(|style| concat_text::word_ranges(&title_style(style).content).len() + 1)
                .sum(),
            ..PreparationStatus::default()
        };
        state.pending = (!missing.is_empty()).then(|| Request {
            generation,
            project: project.clone(),
            width,
            height,
            styles: missing,
        });
        if state.pending.is_none() || state.running {
            return;
        }
        state.running = true;
        drop(state);
        let shared = Arc::clone(&self.preparation);
        let cache = Arc::clone(&self.word_memo);
        let dir = self.dir.clone();
        let failure = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("title-words".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Separate fonts: a long background render must not hold the UI's lock.
                    let renderer = Titles::with_word_cache(dir, cache);
                    loop {
                        // Coalesce fast successive edits before starting the next render.
                        std::thread::sleep(std::time::Duration::from_millis(120));
                        let request = {
                            let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                            match state.pending.take() {
                                Some(request) => request,
                                None => {
                                    state.running = false;
                                    return;
                                }
                            }
                        };
                        let current = || {
                            shared
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .status
                                .generation
                                == request.generation
                        };
                        let mut result = Ok(());
                        for style in &request.styles {
                            if !current() {
                                break;
                            }
                            result = renderer
                                .word_states_progress(
                                    &request.project,
                                    style,
                                    request.width,
                                    request.height,
                                    &mut || !current(),
                                    &mut || {
                                        let mut state =
                                            shared.lock().unwrap_or_else(|e| e.into_inner());
                                        if state.status.generation == request.generation {
                                            state.status.completed += 1;
                                        }
                                    },
                                )
                                .map(|_| ());
                            if result.is_err() {
                                break;
                            }
                        }
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        if state.status.generation == request.generation {
                            state.status.preparing = false;
                            state.status.failed = result.is_err();
                            if let Err(error) = result {
                                log::warn!("word preview preparation: {error}");
                            }
                        }
                    }
                }));
                if result.is_err() {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.running = false;
                    state.pending = None;
                    state.status.preparing = false;
                    state.status.failed = true;
                    log::warn!("word preview worker stopped unexpectedly");
                }
            });
        if let Err(error) = spawned {
            let mut state = failure.lock().unwrap_or_else(|e| e.into_inner());
            state.running = false;
            state.pending = None;
            state.status.preparing = false;
            state.status.failed = true;
            log::warn!("could not start word preview worker: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_project::{Command, Editor};
    use std::time::{Duration, Instant};

    fn project(content: &str, copies: usize) -> Project {
        let mut editor = Editor::new();
        for index in 0..copies {
            editor
                .apply(Command::AddTextClip {
                    above: false,
                    track_id: None,
                    start: index as f64 * 4.0,
                    duration: Some(4.0),
                    offset_y: None,
                    style: Some(TextStyle {
                        content: content.into(),
                        word_reveal: true,
                        word_starts: vec![0.0, 0.3, 0.6],
                        ..Default::default()
                    }),
                })
                .unwrap();
        }
        editor.project().clone()
    }

    fn painter() -> (tempfile::TempDir, Titles) {
        let dir = tempfile::tempdir().unwrap();
        let titles = Titles::new(&AppDirs {
            config: dir.path().join("config"),
            data: dir.path().join("data"),
        });
        (dir, titles)
    }

    fn wait_idle(titles: &Titles) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while titles.preparation.lock().unwrap().running {
            assert!(Instant::now() < deadline, "word worker did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn latest_request_replaces_pending_work_and_duplicate_styles_count_once() {
        let (_dir, titles) = painter();
        // Hold dispatch without a timing assumption about how fast the renderer is.
        titles.preparation.lock().unwrap().running = true;
        let first = project("one two three", 2);
        titles.request_words(&first, 64, 96);
        let initial = titles.preparation_status();
        assert!(initial.preparing);
        assert_eq!(initial.total, 4);
        titles.request_words(&first, 64, 96);
        assert_eq!(initial.generation, titles.preparation_status().generation);
        titles.request_words(&project("four five six", 1), 64, 96);
        let state = titles.preparation.lock().unwrap();
        assert!(state.status.generation > initial.generation);
        assert_eq!(
            state.pending.as_ref().unwrap().styles[0].content,
            "four five six"
        );
        drop(state);
        titles.cancel_preparation();
        assert!(!titles.preparation_status().preparing);
        assert!(titles.preparation.lock().unwrap().pending.is_none());
    }

    #[test]
    fn preview_is_static_until_ready_and_worker_does_not_hold_ui_fonts() {
        let (_dir, titles) = painter();
        let project = project("one two three", 1);
        let before = serde_json::to_value(&project).unwrap();
        titles.preparation.lock().unwrap().running = true;
        let preview = titles.clips_preview(&project, 64, 96, true);
        assert!(preview[0].clip.title_reveal.is_none());
        assert!(preview[0].frame.is_some());
        assert!(titles.preparation_status().preparing);
        {
            let mut state = titles.preparation.lock().unwrap();
            state.running = false;
            state.signature.clear();
        }
        let fonts = titles.fonts.lock().unwrap();
        titles.request_words(&project, 64, 96);
        wait_idle(&titles);
        drop(fonts);
        let status = titles.preparation_status();
        assert!(!status.failed);
        assert_eq!((status.completed, status.total), (4, 4));
        let ready = titles.clips_preview(&project, 64, 96, true);
        let reveal = ready[0].clip.title_reveal.as_ref().unwrap();
        assert_eq!(reveal.paths.len(), 4);
        assert_eq!(reveal.starts, vec![0.0, 0.3, 0.6]);
        let export = titles.clips(&project, 64, 96);
        assert_eq!(export[0].clip.title_reveal.as_ref(), Some(reveal));
        assert_eq!(serde_json::to_value(&project).unwrap(), before);
    }

    #[test]
    fn cancellation_leaves_no_partial_png_and_export_prepares_all_states() {
        let (_dir, titles) = painter();
        let project = project("one two three", 1);
        let style = project.active().clips[0].text.as_ref().unwrap();
        let count = std::cell::Cell::new(0);
        let result = titles.word_states_progress(
            &project,
            style,
            64,
            96,
            &mut || count.get() >= 1,
            &mut || count.set(count.get() + 1),
        );
        assert!(result.is_err());
        assert!(
            titles
                .cached_words(key_of(&project, Painting::Title(style), 64, 96))
                .is_none()
        );
        assert!(std::fs::read_dir(&titles.dir).unwrap().all(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext != "tmp")
        }));
        let exported = titles.clips(&project, 64, 96);
        assert_eq!(
            exported[0].clip.title_reveal.as_ref().unwrap().paths.len(),
            4
        );
    }

    #[test]
    fn disabling_cancels_work_and_export_does_not_use_static_preview_fallback() {
        let (_dir, titles) = painter();
        let project = project("one two three", 1);
        titles.request_words(&project, 64, 96);
        // Export may race the preview worker; unique atomic writes must stay valid.
        let export = titles.clips(&project, 64, 96);
        assert!(export[0].clip.title_reveal.is_some());
        let mut disabled = project.clone();
        disabled
            .active_mut()
            .clip_mut(&project.active().clips[0].id)
            .unwrap()
            .text
            .as_mut()
            .unwrap()
            .word_reveal = false;
        let preview = titles.clips_preview(&disabled, 64, 96, true);
        assert!(preview[0].clip.title_reveal.is_none());
        assert!(!titles.preparation_status().preparing);
        wait_idle(&titles);
        assert!(
            titles.clips_preview(&disabled, 64, 96, true)[0]
                .clip
                .title_reveal
                .is_none()
        );
    }

    #[test]
    fn failed_preparation_is_reported_without_an_automatic_retry_loop() {
        let (_dir, titles) = painter();
        let project = project("one two three", 1);
        std::fs::create_dir_all(titles.dir.parent().unwrap()).unwrap();
        std::fs::write(&titles.dir, b"not a directory").unwrap();
        titles.request_words(&project, 64, 96);
        wait_idle(&titles);
        let status = titles.preparation_status();
        assert!(status.failed);
        assert!(!status.preparing);
        titles.request_words(&project, 64, 96);
        assert_eq!(titles.preparation_status().generation, status.generation);
        assert!(!titles.preparation.lock().unwrap().running);
    }
}
