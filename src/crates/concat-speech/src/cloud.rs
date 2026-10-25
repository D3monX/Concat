// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Opt-in OpenRouter transcription. No retries, temporary audio files,
//! transcript logging, or local-to-cloud fallback. Only the decoded clip window
//! leaves the machine. Dropping the HTTP future cancels in-flight network work;
//! it cannot undo bytes already sent or a provider's charge.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use concat_media::{AudioDecoder, AudioOptions, SampleFormat};
use serde::Deserialize;

use crate::JobStage;
use crate::transcribe::{Segment, TokenTiming, TranscribeRequest};
use concat_host::AppDirs;

/// The sole cloud model supported by Concat.
pub const MODEL: &str = "microsoft/mai-transcribe-2";
/// Concat's conservative per-request ceiling, not a provider guarantee.
pub const MAX_SECONDS: f64 = 300.0;
const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const ENDPOINT: &str = "https://openrouter.ai/api/v1/audio/transcriptions";
const TIMEOUT: Duration = Duration::from_secs(90);
const RATE: usize = 16_000;

fn credential(dirs: &AppDirs) -> Result<reqwest::header::HeaderValue, String> {
    let key = crate::credentials::read(dirs).map_err(|_| {
        "OpenRouter credential is not available; open the API key settings".to_owned()
    })?;
    let bearer = zeroize::Zeroizing::new(format!("Bearer {}", key.expose()));
    let mut value = reqwest::header::HeaderValue::from_str(&bearer)
        .map_err(|_| "Invalid OpenRouter credential".to_owned())?;
    value.set_sensitive(true);
    Ok(value)
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("transcription cancelled".to_owned())
    } else {
        Ok(())
    }
}

fn validate_request(request: &TranscribeRequest) -> Result<(), String> {
    if !request.cloud_consent {
        return Err("Explicit consent is required before sending audio to OpenRouter".to_owned());
    }
    if request.model_id != MODEL {
        return Err("OpenRouter supports only microsoft/mai-transcribe-2".to_owned());
    }
    if !request.source_start.is_finite()
        || request.source_start < 0.0
        || !(request.source_start + request.window).is_finite()
        || !request.window.is_finite()
        || request.window <= 0.0
        || request.window > MAX_SECONDS
    {
        return Err(
            "Cloud transcription needs a finite audio window of at most 300 seconds".to_owned(),
        );
    }
    Ok(())
}

pub(crate) fn transcribe(
    dirs: &AppDirs,
    request: &TranscribeRequest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(JobStage),
) -> Result<Vec<Segment>, String> {
    validate_request(request)?;
    check_cancel(cancel)?;
    let authorization = credential(dirs)?;
    progress(JobStage::Reading);
    let mut decoder = AudioDecoder::open(
        &request.path,
        &AudioOptions {
            start: Some(request.source_start),
            duration: Some(request.window),
            filters: Vec::new(),
            rate: RATE as u32,
            channels: 1,
            format: SampleFormat::F32,
            stream: request.audio_stream.map(|s| s as usize),
        },
    )
    .map_err(|_| "Could not decode the cloud transcription window".to_owned())?;
    let mut samples = Vec::new();
    let max_samples = (request.window * RATE as f64).floor() as usize;
    loop {
        check_cancel(cancel)?;
        let Some(frame) = decoder
            .next_f32()
            .map_err(|_| "Could not read the cloud transcription window".to_owned())?
        else {
            break;
        };
        // A decoder packet may cross the requested source out-point. Never
        // upload samples beyond the window the user selected.
        let take = frame.len().min(max_samples.saturating_sub(samples.len()));
        if 44 + (samples.len() + take) * 2 > MAX_AUDIO_BYTES {
            return Err("Decoded cloud audio exceeds the size or duration limit".to_owned());
        }
        samples.extend_from_slice(&frame[..take]);
        if samples.len() == max_samples {
            break;
        }
    }
    if samples.is_empty() {
        return Err("The clip has no audio".to_owned());
    }
    let skipped = if request.skip_leading_silence {
        crate::leading_silence::leading_samples(&samples)
    } else {
        0
    };
    let duration = (samples.len() - skipped) as f64 / RATE as f64;
    let wav = wav(&samples[skipped..])?;
    check_cancel(cancel)?;
    let body = request_body(&wav);
    log::info!(
        "OpenRouter request: model={} response_format=verbose_json timestamp_granularities=word requested_seconds={:.6} submitted_seconds={:.6} pcm_samples={} audio_bytes={}",
        MODEL,
        request.window,
        duration,
        (wav.len() - 44) / 2,
        wav.len()
    );
    progress(JobStage::Working {
        done: 0.0,
        next: 1.0,
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Could not start the cloud network runtime".to_owned())?;
    let bytes = runtime.block_on(exchange(ENDPOINT, authorization, body, cancel, TIMEOUT))?;
    check_cancel(cancel)?;
    let mut segments = parse_response(&bytes, duration)?;
    restore_offset(&mut segments, skipped as f64 / RATE as f64);
    Ok(segments)
}

fn restore_offset(segments: &mut [Segment], seconds: f64) {
    for segment in segments {
        segment.start += seconds;
        segment.end += seconds;
        for token in &mut segment.tokens {
            token.start += seconds;
            token.end += seconds;
        }
    }
}

fn wav(samples: &[f32]) -> Result<Vec<u8>, String> {
    if samples.is_empty()
        || 44 + samples.len() * 2 > MAX_AUDIO_BYTES
        || samples.iter().any(|s| !s.is_finite())
    {
        return Err("Invalid or oversized decoded cloud audio".to_owned());
    }
    let size = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(size as usize + 44);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(size + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&(RATE as u32).to_le_bytes());
    bytes.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&size.to_le_bytes());
    for sample in samples {
        bytes
            .extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    Ok(bytes)
}

fn request_body(wav: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "model": MODEL,
        "input_audio": {"data": base64::engine::general_purpose::STANDARD.encode(wav), "format": "wav"},
        "response_format": "verbose_json", "timestamp_granularities": ["word"]
    })
}

async fn exchange(
    endpoint: &str,
    authorization: reqwest::header::HeaderValue,
    body: serde_json::Value,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    check_cancel(cancel)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|_| "Could not configure cloud HTTPS".to_owned())?;
    let operation = async {
        let mut response = client
            .post(endpoint)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;
        log::info!(
            "OpenRouter response: http_status={}",
            response.status().as_u16()
        );
        if let Some(id) = response
            .headers()
            .get("x-generation-id")
            .and_then(|value| value.to_str().ok())
            .filter(|id| {
                id.starts_with("gen-")
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
        {
            log::info!("OpenRouter generation_id={id}");
        }
        if !response.status().is_success() {
            // Never echo a remote body: it may repeat audio, text or credentials.
            return Err(match response.status().as_u16() {
                401 | 403 => "OpenRouter rejected the credential",
                402 => "OpenRouter credit is insufficient",
                413 => "OpenRouter rejected the audio size",
                429 => "OpenRouter rate limit reached; retry manually later",
                400 | 422 => "OpenRouter rejected the word timestamp request",
                300..=399 => "OpenRouter redirect refused",
                _ => "OpenRouter service failed; retry manually later",
            }
            .to_owned());
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err("Cloud response exceeds the size limit".to_owned());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err("Cloud response exceeds the size limit".to_owned());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    };
    let cancelled = async {
        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::select! {
        biased;
        _ = cancelled => Err("transcription cancelled".to_owned()),
        result = tokio::time::timeout(timeout, operation) => result.map_err(|_| "Cloud transcription timed out".to_owned())?,
    }
}

fn network_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Cloud transcription timed out"
    } else {
        "Cloud transcription network error"
    }
    .to_owned()
}

#[derive(Deserialize)]
struct Transcript {
    #[serde(default)]
    text: String,
    words: Vec<Word>,
}
#[derive(Deserialize)]
struct Word {
    word: String,
    start: f64,
    end: f64,
}

fn validation_error(code: &str, details: &str) -> String {
    // These details are constructed only from fixed labels and numeric values.
    // Never include remote text, JSON bodies, paths, audio or credentials.
    log::warn!("OpenRouter timestamp validation: code={code} {details}");
    format!("OpenRouter returned invalid or incomplete word timestamps [{code}]: {details}")
}

/// Validate against the actual decoded duration, never the requested length.
/// Preserve matching transcript text/UTF-8 spans, or use the timed word stream
/// when the full text differs. Never synthesize missing word times.
fn parse_response(bytes: &[u8], duration: f64) -> Result<Vec<Segment>, String> {
    const BOUNDARY_TOLERANCE: f64 = 0.05;
    if bytes.len() > MAX_RESPONSE_BYTES || !duration.is_finite() || duration <= 0.0 {
        return Err(validation_error(
            "input-limit",
            &format!(
                "response_bytes={} decoded_seconds={duration:.6}",
                bytes.len()
            ),
        ));
    }
    let raw: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        validation_error(
            "invalid-json",
            &format!(
                "category={:?} line={} column={}",
                error.classify(),
                error.line(),
                error.column()
            ),
        )
    })?;
    let reported_duration = raw.get("duration").and_then(|v| v.as_f64());
    log::info!(
        "OpenRouter response structure: bytes={} text_present={} duration={:?} words={:?} segments={:?}",
        bytes.len(),
        raw.get("text").is_some(),
        reported_duration,
        raw.get("words").and_then(|v| v.as_array()).map(|v| v.len()),
        raw.get("segments")
            .and_then(|v| v.as_array())
            .map(|v| v.len())
    );
    // Provider duration is metadata, not the clock against which word times
    // are checked. Some services omit it or count only recognized speech.
    if reported_duration.is_none_or(|n| !n.is_finite() || (n - duration).abs() > 0.15) {
        log::warn!(
            "OpenRouter duration metadata differs or is absent: provider_seconds={reported_duration:?} submitted_seconds={duration:.6}; using decoded audio bounds"
        );
    }
    if raw.get("text").is_some_and(|v| !v.is_string()) {
        return Err(validation_error("text-field", "text is not a string"));
    }
    let words = raw.get("words").and_then(|v| v.as_array())
        .ok_or_else(|| validation_error("words-field", "top-level words array is missing or invalid; requested verbose_json with word timestamps"))?;
    if words.len() > 20_000 {
        return Err(validation_error(
            "word-limit",
            &format!("words={} limit=20000", words.len()),
        ));
    }
    for (index, word) in words.iter().enumerate() {
        if word.get("word").and_then(|v| v.as_str()).is_none() {
            return Err(validation_error(
                "word-text-field",
                &format!("word={} missing string field", index + 1),
            ));
        }
        for field in ["start", "end"] {
            if word.get(field).and_then(|v| v.as_f64()).is_none() {
                return Err(validation_error(
                    "word-time-field",
                    &format!("word={} field={field} missing or not numeric", index + 1),
                ));
            }
        }
    }
    let mut transcript: Transcript = serde_json::from_value(raw).map_err(|_| {
        validation_error(
            "response-schema",
            "structured transcript could not be decoded",
        )
    })?;
    if transcript.words.is_empty() {
        return if transcript.text.trim().is_empty() {
            Ok(Vec::new())
        } else {
            Err(validation_error(
                "words-empty",
                "transcript contains text but words array is empty",
            ))
        };
    }
    let mut previous_start = 0.0;
    for (index, word) in transcript.words.iter_mut().enumerate() {
        if word.word.trim().is_empty()
            || !word.start.is_finite()
            || !word.end.is_finite()
            || word.end <= word.start
        {
            return Err(validation_error(
                "word-interval",
                &format!(
                    "word={} empty={} start={:.6} end={:.6}",
                    index + 1,
                    word.word.trim().is_empty(),
                    word.start,
                    word.end
                ),
            ));
        }
        if word.start < -BOUNDARY_TOLERANCE || word.end > duration + BOUNDARY_TOLERANCE {
            return Err(validation_error(
                "word-outside-audio",
                &format!(
                    "word={} start={:.6} end={:.6} submitted_seconds={duration:.6} boundary_tolerance_seconds={BOUNDARY_TOLERANCE:.6}",
                    index + 1,
                    word.start,
                    word.end
                ),
            ));
        }
        if word.start < 0.0 || word.end > duration {
            log::warn!(
                "OpenRouter boundary adjustment: word={} start={:.6} end={:.6} submitted_seconds={duration:.6}",
                index + 1,
                word.start,
                word.end
            );
            word.start = word.start.max(0.0);
            word.end = word.end.min(duration);
        }
        if word.end <= word.start {
            return Err(validation_error(
                "word-interval",
                &format!("word={} has no positive interval inside audio", index + 1),
            ));
        }
        if word.start < previous_start {
            return Err(validation_error(
                "word-order",
                &format!(
                    "word={} start={:.6} previous_start={previous_start:.6}",
                    index + 1,
                    word.start
                ),
            ));
        }
        // Measured words may overlap (including coarticulation); do not move
        // their onsets just to manufacture nonoverlapping word intervals.
        previous_start = word.start;
    }
    let mut text = transcript.text.trim().to_owned();
    let spans = match transcript_spans(&text, &transcript.words) {
        Some(spans) => spans,
        None => {
            // The timed word stream is authoritative. Keep every timed word
            // and its measured clock rather than guessing alignment for a
            // differently normalized full transcript (case/numbers/etc.).
            log::warn!(
                "OpenRouter text source=timed-words: transcript_bytes={} words={}; full transcript differs from timed words",
                text.len(),
                transcript.words.len()
            );
            text.clear();
            transcript
                .words
                .iter()
                .map(|word| {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    let start = text.len();
                    text.push_str(word.word.trim());
                    (start, text.len())
                })
                .collect()
        }
    };
    log::info!(
        "OpenRouter caption grouping: max_words={} max_chars={} max_seconds={} pause_seconds={} short_pause_seconds={} preferred_min_words={}",
        CAPTION_MAX_WORDS,
        CAPTION_MAX_CHARS,
        CAPTION_MAX_SECONDS,
        CAPTION_PAUSE,
        CAPTION_SHORT_PAUSE,
        CAPTION_MIN_WORDS
    );
    let mut segments = Vec::new();
    let mut first = 0;
    while first < transcript.words.len() {
        let i = natural_caption_end(&text, &transcript.words, &spans, first);
        let initial = if first == 0 { 0 } else { spans[first].0 };
        let boundary = if i + 1 == spans.len() {
            text.len()
        } else {
            spans[i + 1].0
        };
        let raw = &text[initial..boundary];
        let base = initial + raw.len() - raw.trim_start().len();
        let content = raw.trim();
        let tokens = (first..=i)
            .map(|j| TokenTiming {
                byte_start: spans[j].0 - base,
                byte_end: if j == i {
                    content.len()
                } else {
                    spans[j].1 - base
                },
                start: transcript.words[j].start,
                end: transcript.words[j].end,
            })
            .collect();
        segments.push(Segment {
            start: transcript.words[first].start,
            end: transcript.words[first..=i]
                .iter()
                .map(|w| w.end)
                .fold(0.0, f64::max),
            text: content.to_owned(),
            tokens,
        });
        first = i + 1;
    }
    Ok(segments)
}

// The natural-boundary policy selected in the local subtitle laboratory.
// Limits constrain grouping only: token clocks are never distributed, scaled,
// or synthesised. A single oversized provider word stays whole.
const CAPTION_MAX_WORDS: usize = 8;
const CAPTION_MAX_CHARS: usize = 55;
const CAPTION_MAX_SECONDS: f64 = 3.5;
const CAPTION_PAUSE: f64 = 0.5;
const CAPTION_SHORT_PAUSE: f64 = 0.25;
const CAPTION_MIN_WORDS: usize = 3;

fn caption_word_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count()
}

fn caption_content<'a>(
    text: &'a str,
    spans: &[(usize, usize)],
    first: usize,
    last: usize,
) -> &'a str {
    let start = if first == 0 { 0 } else { spans[first].0 };
    let end = spans.get(last + 1).map_or(text.len(), |span| span.0);
    text[start..end].trim()
}

fn caption_sentence_end(text: &str) -> bool {
    let text = caption_without_closing_quotes(text);
    let token = text.split_whitespace().last().unwrap_or("").to_lowercase();
    if let Some(stem) = token.strip_suffix('.')
        && ((stem.chars().count() == 1 && stem.chars().all(char::is_alphabetic))
            || matches!(
                stem,
                "sig"
                    | "sigg"
                    | "sig.ra"
                    | "dott"
                    | "dr"
                    | "prof"
                    | "ecc"
                    | "es"
                    | "pag"
                    | "art"
                    | "mr"
                    | "mrs"
                    | "ms"
            ))
    {
        return false;
    }
    text.ends_with(['.', '!', '?', '…', '。', '！', '？'])
}

fn caption_without_closing_quotes(text: &str) -> &str {
    text.trim_end()
        .trim_end_matches(['"', '\'', '»', '”', '’', ')', ']', '}'])
}

fn natural_caption_end(
    text: &str,
    words: &[Word],
    spans: &[(usize, usize)],
    first: usize,
) -> usize {
    let mut maximum = first;
    let mut end = words[first].end;
    for (i, word) in words.iter().enumerate().skip(first) {
        end = end.max(word.end);
        let content = caption_content(text, spans, first, i);
        if caption_word_count(content) > CAPTION_MAX_WORDS
            || content.chars().count() > CAPTION_MAX_CHARS
            || end - words[first].start > CAPTION_MAX_SECONDS + 1e-9
        {
            if i == first {
                log::warn!(
                    "OpenRouter caption grouping: word={} exceeds a grouping limit; retaining measured word intact",
                    i + 1
                );
            }
            break;
        }
        maximum = i;
    }
    let horizon = (first..=maximum)
        .find(|&i| caption_sentence_end(caption_content(text, spans, first, i)))
        .unwrap_or(maximum);
    let mut cut = maximum;
    let mut best = f64::NEG_INFINITY;
    for i in first..=horizon {
        let content = caption_content(text, spans, first, i);
        let gap = words
            .get(i + 1)
            .map_or(0.0, |next| next.start - words[i].end);
        let priority = if caption_sentence_end(content) {
            100.0
        } else if gap >= CAPTION_PAUSE {
            80.0
        } else if caption_without_closing_quotes(content)
            .ends_with([',', ';', ':', '，', '；', '：'])
        {
            50.0
        } else if gap >= CAPTION_SHORT_PAUSE {
            20.0
        } else if i == horizon {
            0.0
        } else {
            continue;
        };
        let mut score = priority + 10.0 * (i - first + 1) as f64 / (horizon - first + 1) as f64;
        if caption_word_count(content) < CAPTION_MIN_WORDS && i < horizon {
            score -= 65.0;
        }
        // Same orphan lookahead as the laboratory: bounded even when a
        // provider has a long stream with no sentence punctuation.
        let mut tail_end = i + 1;
        while tail_end < words.len()
            && tail_end - i <= CAPTION_MAX_WORDS
            && !caption_sentence_end(caption_content(text, spans, i + 1, tail_end))
        {
            tail_end += 1;
        }
        if tail_end < words.len() && tail_end - i <= CAPTION_MAX_WORDS {
            let remaining = caption_word_count(caption_content(text, spans, i + 1, tail_end));
            if remaining > 0 && remaining < CAPTION_MIN_WORDS {
                score -= 55.0;
            }
        }
        if score > best {
            best = score;
            cut = i;
        }
    }
    cut
}

// Keep the full transcript and punctuation when its word stream matches.
fn transcript_spans(text: &str, words: &[Word]) -> Option<Vec<(usize, usize)>> {
    let mut spans = Vec::with_capacity(words.len());
    let mut cursor = 0;
    for word in words {
        let value = word.word.trim();
        let start = cursor + text[cursor..].find(value)?;
        if text[cursor..start].chars().any(char::is_alphanumeric) {
            return None;
        }
        let end = start + value.len();
        spans.push((start, end));
        cursor = end;
    }
    if text[cursor..].chars().any(char::is_alphanumeric) {
        return None;
    }
    Some(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, mpsc};
    const ITALIAN: &[u8] = include_bytes!("../fixtures/openrouter/italian.json");

    fn fixture() -> Value {
        serde_json::from_slice(ITALIAN).unwrap()
    }
    fn parsed(value: Value) -> Result<Vec<Segment>, String> {
        parse_response(&serde_json::to_vec(&value).unwrap(), 5.0)
    }
    fn request() -> TranscribeRequest {
        TranscribeRequest {
            backend: crate::transcribe::Backend::OpenRouter,
            cloud_consent: true,
            path: "not-a-real-personal-file.wav".into(),
            source_start: 2.0,
            window: 5.0,
            audio_stream: None,
            model_id: MODEL.into(),
            skip_leading_silence: false,
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn italian_groups_phrases_and_keeps_word_bytes_and_absolute_window_times() {
        let result = parse_response(ITALIAN, 5.0).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].text, "Ciao, perché restare soli?");
        assert_eq!(result[1].text, "Siamo qui.");
        assert_eq!(result[0].tokens.len(), 4);
        assert_eq!(result[1].tokens[0].start, 3.4);
        let token = &result[0].tokens[1];
        assert_eq!(
            result[0].text[token.byte_start..token.byte_end].trim(),
            "perché"
        );
        assert_eq!((token.start, token.end), (0.8, 1.2));
    }
    #[test]
    fn leading_silence_offset_restores_phrase_and_word_clocks_together() {
        let mut segments = parse_response(ITALIAN, 5.0).unwrap();
        let original = segments.clone();
        restore_offset(&mut segments, 4.01);
        for (after, before) in segments.iter().zip(original) {
            assert!((after.start - before.start - 4.01).abs() < 1e-9);
            assert!((after.end - before.end - 4.01).abs() < 1e-9);
            assert_eq!(after.text, before.text);
            for (after, before) in after.tokens.iter().zip(before.tokens) {
                assert_eq!(after.byte_start, before.byte_start);
                assert_eq!(after.byte_end, before.byte_end);
                assert!((after.start - before.start - 4.01).abs() < 1e-9);
                assert!((after.end - before.end - 4.01).abs() < 1e-9);
            }
        }
    }
    #[test]
    fn unicode_without_spaces_is_kept_exactly() {
        let result = parsed(json!({"text":"你好世界。","duration":5,"words":[
            {"word":"你好","start":0.2,"end":0.8},{"word":"世界","start":0.9,"end":1.4}]}))
        .unwrap();
        assert_eq!(result[0].text, "你好世界。");
        assert_eq!(result[0].tokens[1].byte_start, "你好".len());
    }
    #[test]
    fn text_without_word_times_is_never_accepted() {
        let mut value = fixture();
        value.as_object_mut().unwrap().remove("words");
        let error = parsed(value).unwrap_err();
        assert!(error.contains("[words-field]"));
        assert!(error.contains("requested verbose_json"));
        let mut empty = fixture();
        empty["words"] = json!([]);
        assert!(parsed(empty).is_err());
        assert!(parse_response(b"not json", 5.0).is_err());
    }

    #[test]
    fn provider_duration_metadata_does_not_override_decoded_word_bounds() {
        let expected = parse_response(ITALIAN, 5.0).unwrap();
        for duration in [json!(null), json!(0), json!(4.5), json!(5.2)] {
            let mut value = fixture();
            value["duration"] = duration;
            assert_eq!(parsed(value).unwrap(), expected);
        }
        let mut missing = fixture();
        missing.as_object_mut().unwrap().remove("duration");
        assert_eq!(parsed(missing).unwrap(), expected);
    }

    #[test]
    fn overlapping_words_keep_measured_onsets_and_the_full_phrase_end() {
        let result = parsed(json!({"text":"uno due","words":[
            {"word":"uno","start":0.1,"end":2.0},
            {"word":"due","start":0.5,"end":1.0}]}))
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!((result[0].start, result[0].end), (0.1, 2.0));
        assert_eq!(
            (result[0].tokens[0].start, result[0].tokens[1].start),
            (0.1, 0.5)
        );
        assert_eq!(result[0].tokens[1].end, 1.0);
    }

    #[test]
    fn small_boundary_drift_is_clipped_without_retiming_interior_words() {
        let mut value = fixture();
        value["words"][0]["start"] = json!(-0.02);
        value["words"][5]["end"] = json!(5.02);
        let result = parsed(value).unwrap();
        assert_eq!(result[0].start, 0.0);
        assert_eq!(result[0].tokens[1].start, 0.8);
        assert_eq!(result.last().unwrap().end, 5.0);
    }

    #[test]
    fn different_full_text_uses_all_timed_words_without_inventing_times() {
        let result = parsed(json!({"text":"21!","words":[
            {"word":"venti","start":0.2,"end":0.6},
            {"word":"uno!","start":0.7,"end":1.0}]}))
        .unwrap();
        assert_eq!(result[0].text, "venti uno!");
        assert_eq!(result[0].tokens.len(), 2);
        assert_eq!(result[0].tokens[1].start, 0.7);
        let mut value = fixture();
        value.as_object_mut().unwrap().remove("text");
        let result = parsed(value).unwrap();
        assert_eq!(result.iter().map(|s| s.tokens.len()).sum::<usize>(), 6);
    }

    #[test]
    fn diagnostics_never_echo_untrusted_field_values() {
        let mut value = fixture();
        value["words"][0]["start"] = json!("private-diagnostic-marker");
        let error = parsed(value.clone()).unwrap_err();
        assert!(error.contains("[word-time-field]"));
        assert!(!error.contains("private-diagnostic-marker"));
        value["words"][0]["start"] = json!(0.3);
        value["words"][0]["word"] = json!({"private-diagnostic-marker":true});
        let error = parsed(value).unwrap_err();
        assert!(error.contains("[word-text-field]"));
        assert!(!error.contains("private-diagnostic-marker"));
    }

    #[test]
    fn invalid_intervals_and_backwards_onsets_remain_errors_with_numeric_details() {
        for (pointer, value, code) in [
            ("/words/0/start", json!(-0.1), "word-outside-audio"),
            ("/words/0/end", json!(0.3), "word-interval"),
            ("/words/0/end", json!(5.1), "word-outside-audio"),
            ("/words/1/start", json!(0.2), "word-order"),
            ("/words/1/start", json!(null), "word-time-field"),
            ("/words/1/word", json!(""), "word-interval"),
        ] {
            let mut value_json = fixture();
            *value_json.pointer_mut(pointer).unwrap() = value;
            let error = parsed(value_json).unwrap_err();
            assert!(error.contains(&format!("[{code}]")), "{error}");
        }
        assert!(parse_response(ITALIAN, f64::INFINITY).is_err());
    }
    #[test]
    fn silence_must_be_explicit_and_structured() {
        assert!(
            parsed(json!({"duration":5,"text":"", "words":[]}))
                .unwrap()
                .is_empty()
        );
        assert!(parsed(json!({"duration":5,"text":"omitted speech", "words":[]})).is_err());
    }
    #[test]
    fn grouping_limits_phrases_without_discarding_words() {
        let words: Vec<_> = (0..40)
            .map(|i| json!({"word":"parola","start":i as f64*0.1,"end":i as f64*0.1+0.08}))
            .collect();
        let result =
            parsed(json!({"text":vec!["parola";40].join(" "),"duration":5,"words":words})).unwrap();
        assert!(result.len() >= 3);
        assert_eq!(result.iter().map(|s| s.tokens.len()).sum::<usize>(), 40);
        assert!(result.iter().all(|s| s.tokens.len() <= 8
            && s.text.chars().count() <= 55
            && s.end - s.start <= 3.5));
    }

    fn timed_phrase(text: &str, step: f64, long_pause_after: Option<usize>) -> Value {
        let words: Vec<_> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, word)| {
                let pause = if long_pause_after.is_some_and(|at| i > at) {
                    1.0
                } else {
                    0.0
                };
                let start = 0.2 + i as f64 * step + pause;
                json!({"word":word,"start":start,"end":start + step * 0.8})
            })
            .collect();
        json!({"text":text,"words":words})
    }

    #[test]
    fn natural_grouping_prefers_commas_and_avoids_a_last_word_orphan() {
        let text = "Non puoi continuare a spingere, finirai col fondere, distruggerti.";
        let input = timed_phrase(text, 0.15, Some(7));
        let bytes = serde_json::to_vec(&input).unwrap();
        // Compare the exact timestamps received over JSON. Serialising a
        // computed float can change its last bit when it is parsed again.
        let input: Value = serde_json::from_slice(&bytes).unwrap();
        let result = parse_response(&bytes, 5.0).unwrap();
        assert_eq!(
            result.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            [
                "Non puoi continuare a spingere,",
                "finirai col fondere, distruggerti."
            ]
        );
        let tokens: Vec<_> = result.iter().flat_map(|s| &s.tokens).collect();
        let words = input["words"].as_array().unwrap();
        assert_eq!(tokens.len(), words.len());
        for (token, word) in tokens.iter().zip(words) {
            assert_eq!(token.start, word["start"].as_f64().unwrap());
            assert_eq!(token.end, word["end"].as_f64().unwrap());
        }
        for segment in result {
            assert_eq!(segment.start, segment.tokens[0].start);
            assert_eq!(
                segment.end,
                segment.tokens.iter().map(|t| t.end).fold(0.0, f64::max)
            );
            for token in &segment.tokens {
                assert!(segment.text.is_char_boundary(token.byte_start));
                assert!(segment.text.is_char_boundary(token.byte_end));
            }
        }
    }

    #[test]
    fn natural_grouping_uses_significant_and_short_pauses_before_the_limit() {
        let text = "uno due tre quattro cinque sei sette otto nove dieci undici dodici";
        for gap in [0.3, 0.6] {
            let mut input = timed_phrase(text, 0.1, None);
            let words = input["words"].as_array_mut().unwrap();
            for word in words.iter_mut().skip(4) {
                for field in ["start", "end"] {
                    word[field] = json!(word[field].as_f64().unwrap() + gap);
                }
            }
            let result = parsed(input).unwrap();
            assert_eq!(result[0].text, "uno due tre quattro");
        }
    }

    #[test]
    fn natural_grouping_enforces_chars_and_duration_without_truncating_words() {
        for (text, step) in [
            (
                "abcdefghij klmnopqrst uvwxyzabcd efghijklmn opqrstuvwx yzabcdefgh ijklmnopqr",
                0.1,
            ),
            ("uno due tre quattro cinque sei sette otto nove dieci", 0.8),
        ] {
            let result = parse_response(
                &serde_json::to_vec(&timed_phrase(text, step, None)).unwrap(),
                20.0,
            )
            .unwrap();
            assert_eq!(
                result
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                text
            );
            assert!(result.iter().all(|s| caption_word_count(&s.text) <= 8
                && s.text.chars().count() <= 55
                && s.end - s.start <= 3.5));
        }
        let long_word = "à".repeat(56);
        let result = parsed(timed_phrase(&long_word, 0.1, None)).unwrap();
        assert_eq!(result[0].text, long_word);
        assert_eq!(result[0].tokens.len(), 1);
    }

    #[test]
    fn natural_grouping_recognizes_ellipsis_quotes_and_abbreviations() {
        for text in ["Bene…", "Bene...", "Bene!\"", "Bene?»", "Bene.)", "你好。"] {
            assert!(caption_sentence_end(text), "{text}");
        }
        for text in ["sig.", "Dott.", "prof.", "A.", "3,14", "continua,"] {
            assert!(!caption_sentence_end(text), "{text}");
        }
        let text = "Lui disse: «Siamo già qui!» Poi uscì dalla stanza.";
        let result = parsed(timed_phrase(text, 0.1, None)).unwrap();
        assert_eq!(result[0].text, "Lui disse: «Siamo già qui!»");
        assert_eq!(result[1].text, "Poi uscì dalla stanza.");
    }
    #[test]
    fn consent_model_window_and_prestart_cancel_fail_before_file_or_network_access() {
        let mut request = request();
        request.cloud_consent = false;
        assert!(validate_request(&request).is_err());
        assert!(
            transcribe(
                &AppDirs::under(std::path::Path::new("unused-cloud")),
                &request,
                &AtomicBool::new(false),
                &mut |_| {}
            )
            .unwrap_err()
            .contains("consent")
        );
        request.cloud_consent = true;
        request.model_id = "openai/whisper-1".into();
        assert!(validate_request(&request).is_err());
        request.model_id = MODEL.into();
        for window in [0.0, -1.0, 300.1, f64::NAN, f64::INFINITY] {
            request.window = window;
            assert!(validate_request(&request).is_err());
        }
        request.window = 5.0;
        assert_eq!(
            transcribe(
                &AppDirs::under(std::path::Path::new("unused-cloud")),
                &request,
                &AtomicBool::new(true),
                &mut |_| {}
            )
            .unwrap_err(),
            "transcription cancelled"
        );
    }
    #[test]
    fn wav_and_request_contract_have_only_the_approved_model_and_required_word_format() {
        let audio = wav(&[0.0, 1.0, -1.0]).unwrap();
        assert_eq!(&audio[..4], b"RIFF");
        assert_eq!(&audio[8..12], b"WAVE");
        assert_eq!(audio.len(), 50);
        assert_eq!(u32::from_le_bytes(audio[24..28].try_into().unwrap()), 16000);
        let body = request_body(&audio);
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["response_format"], "verbose_json");
        assert_eq!(body["timestamp_granularities"], json!(["word"]));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body["input_audio"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, audio);
        assert!(wav(&[f32::NAN]).is_err());
        assert!(wav(&[]).is_err());
        assert!(wav(&vec![0.0; MAX_AUDIO_BYTES / 2]).is_err());
    }

    // A real HTTP client against loopback, never OpenRouter. Captures the wire
    // request so request construction, response limits and cancellation are
    // exercised together rather than mocked out of the transport itself.
    fn server(
        status: u16,
        body: Vec<u8>,
        delay: Duration,
        length: Option<usize>,
    ) -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap_or(0);
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                    let size: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if data.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8(data).unwrap());
            std::thread::sleep(delay);
            let headers = format!(
                "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                length.unwrap_or(body.len())
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(&body);
        });
        (format!("http://{address}/audio/transcriptions"), rx, task)
    }
    fn authorization() -> reqwest::header::HeaderValue {
        let mut value = reqwest::header::HeaderValue::from_static("Bearer mock-credential");
        value.set_sensitive(true);
        value
    }
    #[test]
    fn mock_http_success_checks_the_wire_contract() {
        let (url, rx, task) = server(200, ITALIAN.to_vec(), Duration::ZERO, None);
        let result = runtime()
            .block_on(exchange(
                &url,
                authorization(),
                request_body(&wav(&[0.0]).unwrap()),
                &AtomicBool::new(false),
                Duration::from_secs(2),
            ))
            .unwrap();
        assert_eq!(parse_response(&result, 5.0).unwrap().len(), 2);
        let request = rx.recv().unwrap();
        assert!(request.starts_with("POST /audio/transcriptions"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["response_format"], "verbose_json");
        assert_eq!(body["timestamp_granularities"], json!(["word"]));
        assert!(!request.contains("not-a-real-personal-file"));
        task.join().unwrap();
    }
    #[test]
    fn mock_http_statuses_never_echo_remote_secrets_or_retry() {
        for status in [302, 400, 401, 402, 403, 413, 422, 429, 500] {
            let (url, _rx, task) = server(
                status,
                b"remote private transcript mock-credential".to_vec(),
                Duration::ZERO,
                None,
            );
            let error = runtime()
                .block_on(exchange(
                    &url,
                    authorization(),
                    json!({}),
                    &AtomicBool::new(false),
                    Duration::from_secs(2),
                ))
                .unwrap_err();
            assert!(!error.contains("mock-credential") && !error.contains("private"));
            let expected = match status {
                302 => "redirect",
                400 | 422 => "word timestamp",
                401 | 403 => "credential",
                402 => "credit",
                413 => "size",
                429 => "rate limit",
                _ => "service failed",
            };
            assert!(error.contains(expected), "{status}: {error}");
            task.join().unwrap();
        }
    }
    #[test]
    fn mock_http_timeout_and_cancellation_drop_the_request() {
        for cancel_run in [false, true] {
            let (url, rx, task) = server(200, ITALIAN.to_vec(), Duration::from_millis(300), None);
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = Arc::clone(&cancel);
            let trigger = std::thread::spawn(move || {
                rx.recv().unwrap();
                if cancel_run {
                    signal.store(true, Ordering::Relaxed);
                }
            });
            let error = runtime()
                .block_on(exchange(
                    &url,
                    authorization(),
                    json!({}),
                    &cancel,
                    Duration::from_millis(150),
                ))
                .unwrap_err();
            assert!(error.contains(if cancel_run { "cancelled" } else { "timed out" }));
            trigger.join().unwrap();
            task.join().unwrap();
        }
    }
    #[test]
    fn mock_http_enforces_response_byte_limit() {
        for length in [Some(MAX_RESPONSE_BYTES + 1), None] {
            let body = if length.is_some() {
                vec![]
            } else {
                vec![b' '; MAX_RESPONSE_BYTES + 1]
            };
            let (url, _rx, task) = server(200, body, Duration::ZERO, length);
            assert!(
                runtime()
                    .block_on(exchange(
                        &url,
                        authorization(),
                        json!({}),
                        &AtomicBool::new(false),
                        Duration::from_secs(2)
                    ))
                    .unwrap_err()
                    .contains("size limit")
            );
            task.join().unwrap();
        }
    }
}
