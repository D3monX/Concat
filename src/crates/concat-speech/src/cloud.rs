// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Opt-in OpenRouter transcription. No retries, credential files, audio files,
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

/// The sole cloud model supported by Concat.
pub const MODEL: &str = "microsoft/mai-transcribe-2";
/// Concat's conservative per-request ceiling, not a provider guarantee.
pub const MAX_SECONDS: f64 = 300.0;
const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const ENDPOINT: &str = "https://openrouter.ai/api/v1/audio/transcriptions";
const TIMEOUT: Duration = Duration::from_secs(90);
const RATE: usize = 16_000;

/// Whether a process credential is configured. No key crosses the GUI model,
/// project serialization, API/MCP messages or logs. Concat never writes it.
pub fn credential_available() -> bool {
    credential().is_ok()
}

fn credential() -> Result<reqwest::header::HeaderValue, String> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| "Set OPENROUTER_API_KEY in Concat's process environment".to_owned())?;
    if key.trim().is_empty() {
        return Err("OpenRouter credential is empty".to_owned());
    }
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", key.trim()))
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
    request: &TranscribeRequest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(JobStage),
) -> Result<Vec<Segment>, String> {
    validate_request(request)?;
    check_cancel(cancel)?;
    let authorization = credential()?;
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
    let duration = samples.len() as f64 / RATE as f64;
    let wav = wav(&samples)?;
    check_cancel(cancel)?;
    let body = request_body(&wav);
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
    parse_response(&bytes, duration)
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
    text: String,
    duration: f64,
    words: Vec<Word>,
}
#[derive(Deserialize)]
struct Word {
    word: String,
    start: f64,
    end: f64,
}

/// Validate against the actual decoded duration, never the requested length.
/// Keep exact transcript text/UTF-8 byte ranges so Typography and word reveal
/// see the same text. No word-time synthesis or permissive text-only fallback.
fn parse_response(bytes: &[u8], duration: f64) -> Result<Vec<Segment>, String> {
    let invalid = || "OpenRouter returned invalid or incomplete word timestamps".to_owned();
    if bytes.len() > MAX_RESPONSE_BYTES || !duration.is_finite() || duration <= 0.0 {
        return Err(invalid());
    }
    let transcript: Transcript = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if !transcript.duration.is_finite()
        || transcript.duration <= 0.0
        || (transcript.duration - duration).abs() > 0.15
        || transcript.words.len() > 20_000
    {
        return Err(invalid());
    }
    if transcript.words.is_empty() {
        return if transcript.text.trim().is_empty() {
            Ok(Vec::new())
        } else {
            Err(invalid())
        };
    }
    let text = transcript.text.trim();
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut previous_end = 0.0;
    for word in &transcript.words {
        let value = word.word.trim();
        if value.is_empty()
            || !word.start.is_finite()
            || !word.end.is_finite()
            || word.start < 0.0
            || word.end <= word.start
            || word.end > duration + 0.001
            || word.start < previous_end
        {
            return Err(invalid());
        }
        let start = text[cursor..]
            .find(value)
            .map(|n| cursor + n)
            .ok_or_else(invalid)?;
        if text[cursor..start].chars().any(char::is_alphanumeric) {
            return Err(invalid());
        }
        let end = start + value.len();
        spans.push((start, end));
        cursor = end;
        previous_end = word.end;
    }
    if text[cursor..].chars().any(char::is_alphanumeric) {
        return Err(invalid());
    }
    let mut segments = Vec::new();
    let mut first = 0;
    for i in 0..transcript.words.len() {
        let last = i + 1 == transcript.words.len();
        let boundary = if last { text.len() } else { spans[i + 1].0 };
        let chars = text[spans[first].0..boundary].chars().count();
        let pause = !last && transcript.words[i + 1].start - transcript.words[i].end >= 0.8;
        let punctuation = text[spans[i].0..boundary]
            .trim_end()
            .ends_with(['.', '!', '?', '。', '！', '？']);
        let next_too_long = !last
            && (text[spans[first].0..spans[i + 1].1].chars().count() > 84
                || transcript.words[i + 1].end - transcript.words[first].start > 6.0);
        if last || pause || punctuation || next_too_long || i - first >= 13 || chars >= 84 {
            let base = if first == 0 { 0 } else { spans[first].0 };
            let raw = &text[base..boundary];
            let content = raw.trim_end();
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
                end: transcript.words[i].end,
                text: content.to_owned(),
                tokens,
            });
            first = i + 1;
        }
    }
    Ok(segments)
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
    fn unicode_without_spaces_is_kept_exactly() {
        let result = parsed(json!({"text":"你好世界。","duration":5,"words":[
            {"word":"你好","start":0.2,"end":0.8},{"word":"世界","start":0.9,"end":1.4}]}))
        .unwrap();
        assert_eq!(result[0].text, "你好世界。");
        assert_eq!(result[0].tokens[1].byte_start, "你好".len());
    }
    #[test]
    fn text_without_word_times_is_never_accepted() {
        for field in ["words", "duration", "text"] {
            let mut value = fixture();
            value.as_object_mut().unwrap().remove(field);
            assert!(parsed(value).is_err(), "{field}");
        }
        let mut value = fixture();
        value["words"] = json!([]);
        assert!(parsed(value).is_err());
        assert!(parse_response(b"not json", 5.0).is_err());
        assert!(parse_response(b"{\"text\":\"x\",\"duration\":NaN,\"words\":[]}", 5.0).is_err());
    }
    #[test]
    fn invalid_duration_order_overlap_and_text_are_rejected() {
        for (pointer, value) in [
            ("/duration", json!(0)),
            ("/duration", json!(5.2)),
            ("/words/0/start", json!(-0.1)),
            ("/words/0/end", json!(0.3)),
            ("/words/0/end", json!(5.1)),
            ("/words/1/start", json!(0.5)),
            ("/words/1/start", json!(null)),
            ("/words/1/word", json!("")),
            ("/words/1/word", json!("unknown")),
            ("/text", json!("Unrelated text")),
        ] {
            let mut transcript = fixture();
            *transcript.pointer_mut(pointer).unwrap() = value;
            assert!(parsed(transcript).is_err(), "{pointer}");
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
        assert!(
            result
                .iter()
                .all(|s| s.tokens.len() <= 14 && s.text.chars().count() <= 84)
        );
    }
    #[test]
    fn consent_model_window_and_prestart_cancel_fail_before_file_or_network_access() {
        let mut request = request();
        request.cloud_consent = false;
        assert!(validate_request(&request).is_err());
        assert!(
            transcribe(&request, &AtomicBool::new(false), &mut |_| {})
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
            transcribe(&request, &AtomicBool::new(true), &mut |_| {}).unwrap_err(),
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
