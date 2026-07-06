//! Speech-to-text seam for inbound voice notes.
//!
//! A WhatsApp voice note arrives as an `audio` [`crate::model::MediaItem`] (whatsmeow
//! `AudioMessage`, PTT flag set). The forward worker (`crate::forward`) fetches the clip's bytes
//! from GOWA *after* the 200-ack and, when a [`Transcriber`] is configured, folds the transcript
//! into the forwarded `body` while still emitting the audio `media[]` URL — so every downstream
//! channel (hermes/dashi/default) gets the text *and* can fetch the original audio.
//!
//! The engine sits behind the [`Transcriber`] trait so it can be swapped (accuracy, latency, a
//! different multilingual backend) without touching the call sites. The shipped implementation is
//! [`WhisperTranscriber`] (whisper.cpp via `whisper-rs`, with `ogg`+`opus` for voice-note decode),
//! compiled only under the `transcribe` feature so the default build — what `cargo build`/`test`/`clippy` run per
//! `CLAUDE.md` — stays fast, network-free, and model-free. Auto-detection of the spoken language is
//! first-class: [`Transcript::language`] carries the detected language so the agent can reply in
//! kind, which is why the engine must ship a **multilingual** model, never an English-only `.en`
//! build.

use std::{future::Future, pin::Pin, sync::Arc};

use crate::{config::Config, error::DynError};

/// The result of transcribing one audio clip: the recognised `text` (empty when nothing was
/// spoken), the auto-detected spoken `language` (e.g. `"en"`, `"es"`), and the model's confidence
/// in that detection (`0.0..=1.0`) when the engine reports one. Surfacing language + confidence is
/// what makes multi-language first-class: the agent can see e.g. `(es, 0.98)` and reply in kind (or
/// hedge on a low-confidence guess). `PartialEq` is derived for tests; `f32` makes it non-`Eq`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    /// Confidence in `language` (`0.0..=1.0`), when the engine reports a language-detection
    /// probability. `None` when the language was pinned (not detected) or the engine can't report it.
    pub language_confidence: Option<f32>,
}

impl Transcript {
    /// The forwarded transcript block for a non-empty transcript, annotated with the detected
    /// language (and confidence, when known) so the agent can reply in kind or hedge on a shaky
    /// guess — e.g. `[voice note transcript (es, 0.98)]: "hola"`, or `(es)` when confidence is
    /// unknown, or a bare prefix when the language is unknown. Returns `None` for an empty/
    /// whitespace-only transcript (nothing spoken → fall back to today's body).
    pub fn body_block(&self) -> Option<String> {
        let text = self.text.trim();
        if text.is_empty() {
            return None;
        }
        let lang = self
            .language
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let tag = match (lang, self.language_confidence) {
            (Some(lang), Some(confidence)) => format!(" ({lang}, {confidence:.2})"),
            (Some(lang), None) => format!(" ({lang})"),
            (None, _) => String::new(),
        };
        Some(format!("[voice note transcript{tag}]: \"{text}\""))
    }
}

/// A boxed, `Send` future — the hand-rolled async-trait return so no `async_trait` dep enters the
/// default build.
pub type TranscribeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Transcript, DynError>> + Send + 'a>>;

/// Transcribe already-fetched audio bytes. Implementations auto-detect the spoken language and
/// return it in [`Transcript::language`]; `text` is `""` when nothing was spoken. `mime` hints the
/// container (e.g. `"audio/ogg"`).
pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(&'a self, audio: &'a [u8], mime: Option<&'a str>) -> TranscribeFuture<'a>;
}

/// The always-compiled default: transcribes nothing (returns an empty [`Transcript`]). Never wired
/// into the forward path — the worker only calls the trait when a real transcriber is configured —
/// but it lets the trait have a trivial, dependency-free implementor.
#[derive(Debug, Default, Clone)]
pub struct NoopTranscriber;

impl Transcriber for NoopTranscriber {
    fn transcribe<'a>(&'a self, _audio: &'a [u8], _mime: Option<&'a str>) -> TranscribeFuture<'a> {
        Box::pin(async { Ok(Transcript::default()) })
    }
}

/// A canned transcriber for tests: returns a fixed text (and optional language) regardless of input.
/// Always compiled (not `#[cfg(test)]`) so the integration suite (`tests/e2e.rs`, a separate crate)
/// can inject it without pulling in a model or the heavy feature.
#[derive(Debug, Clone)]
pub struct MockTranscriber {
    text: String,
    language: Option<String>,
}

impl MockTranscriber {
    /// A mock that always returns `text` with no detected language.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: None,
        }
    }

    /// A mock that always returns `text` with a fixed detected `language`.
    pub fn with_language(text: impl Into<String>, language: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: Some(language.into()),
        }
    }
}

impl Transcriber for MockTranscriber {
    fn transcribe<'a>(&'a self, _audio: &'a [u8], _mime: Option<&'a str>) -> TranscribeFuture<'a> {
        let transcript = Transcript {
            text: self.text.clone(),
            language: self.language.clone(),
            language_confidence: None,
        };
        Box::pin(async move { Ok(transcript) })
    }
}

/// Build the configured transcriber, or `None` when `SHIM_TRANSCRIBE` is off. Fail-fast on a config
/// that asks for transcription the binary can't provide (feature not compiled, or model missing) —
/// this mirrors the config's fail-fast ethos and matches [`Config::from_env`]'s own preflight so a
/// misconfigured box never silently drops voice notes.
pub fn from_config(config: &Config) -> Result<Option<Arc<dyn Transcriber>>, DynError> {
    if !config.transcribe {
        return Ok(None);
    }
    #[cfg(feature = "transcribe")]
    {
        let model = config
            .whisper_model
            .as_deref()
            .ok_or_else(|| boxed("SHIM_TRANSCRIBE is on but SHIM_WHISPER_MODEL is not set"))?;
        let transcriber = whisper::WhisperTranscriber::load(model, &config.transcribe_lang)?;
        Ok(Some(Arc::new(transcriber)))
    }
    #[cfg(not(feature = "transcribe"))]
    {
        Err(boxed(
            "SHIM_TRANSCRIBE is on but this binary was built without --features transcribe",
        ))
    }
}

#[allow(dead_code)]
fn boxed(message: impl Into<String>) -> DynError {
    Box::<dyn std::error::Error + Send + Sync>::from(message.into())
}

/// The whisper.cpp-backed transcriber. Compiled only under the `transcribe` feature so the C++
/// build + model never weigh down the default (test/lint) build. It loads a **multilingual** ggml
/// model once at startup (shared behind an `Arc<WhisperContext>` — whisper-rs contexts are
/// `Send + Sync`), decodes each clip (Ogg/Opus for a WhatsApp voice note) to mono 16 kHz PCM, runs
/// per-clip language auto-detection to recover the spoken language **and its confidence**, then
/// transcribes.
///
/// Deliberately thin: `whisper-rs` directly (model load + inference + language detection) plus a
/// hand-rolled `ogg`+`opus` decode. No scribble/symphonia — that pulls axum/clap/reqwest+openssl
/// into a service that is meant to stay small. Non-Opus audio (a rare shared mp3/m4a) is not decoded
/// and is forwarded audio-only; WhatsApp *voice notes* are always Ogg/Opus, which this handles.
#[cfg(feature = "transcribe")]
mod whisper {
    use super::{TranscribeFuture, Transcriber, Transcript, boxed};
    use crate::error::DynError;
    use std::{path::Path, sync::Arc};
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    /// Decode `audio` to mono 16 kHz f32 PCM. Only Ogg/Opus (the WhatsApp voice-note container) is
    /// supported; any other container returns an error so the worker forwards the clip audio-only.
    fn decode_samples(audio: Vec<u8>) -> Result<Vec<f32>, DynError> {
        if is_ogg_opus(&audio) {
            return decode_ogg_opus(&audio);
        }
        Err(boxed(
            "unsupported audio container (only Ogg/Opus voice notes are transcribed)",
        ))
    }

    /// True when `audio` is an Ogg stream whose first page carries an `OpusHead` identification
    /// header — i.e. Ogg/Opus (a WhatsApp voice note), as opposed to Ogg/Vorbis or another container.
    fn is_ogg_opus(audio: &[u8]) -> bool {
        audio.starts_with(b"OggS")
            && audio
                .windows(8)
                .take(256)
                .any(|window| window == b"OpusHead")
    }

    /// Demux an Ogg/Opus stream and decode every audio packet to mono 16 kHz f32. The libopus decoder
    /// is created at 16 kHz mono so it resamples/downmixes internally — no separate resample step.
    /// The two Opus setup packets (`OpusHead`, `OpusTags`) carry no audio and are skipped.
    fn decode_ogg_opus(audio: &[u8]) -> Result<Vec<f32>, DynError> {
        let mut reader = ogg::PacketReader::new(std::io::Cursor::new(audio));
        let mut decoder = opus::Decoder::new(16_000, opus::Channels::Mono)
            .map_err(|error| boxed(format!("opus decoder init failed: {error}")))?;
        let mut samples = Vec::new();
        // Max Opus frame is 120 ms; at 16 kHz mono that's 1920 samples — 5760 is a safe ceiling.
        let mut frame = vec![0f32; 5760];
        while let Some(packet) = reader
            .read_packet()
            .map_err(|error| boxed(format!("ogg demux failed: {error}")))?
        {
            let data = &packet.data;
            if data.starts_with(b"OpusHead") || data.starts_with(b"OpusTags") {
                continue;
            }
            match decoder.decode_float(data, &mut frame, false) {
                Ok(count) => samples.extend_from_slice(&frame[..count]),
                // A single corrupt packet shouldn't abort the whole clip.
                Err(error) => tracing::warn!(%error, "skipping undecodable opus packet"),
            }
        }
        Ok(samples)
    }

    /// Owns the loaded model + inference settings. Cheap to clone (`Arc` over the context); the
    /// worker holds one shared instance for the process lifetime.
    pub struct WhisperTranscriber {
        ctx: Arc<WhisperContext>,
        /// Pinned language code, or `None` for per-clip auto-detection (the multi-language default).
        language: Option<String>,
    }

    impl WhisperTranscriber {
        /// Load the multilingual ggml model at `model_path`. `language` is `"auto"`/empty (detect) or
        /// a pinned code. Fails fast if the model can't be loaded.
        pub fn load(model_path: &Path, language: &str) -> Result<Self, DynError> {
            let path = model_path.to_str().ok_or_else(|| {
                boxed(format!(
                    "SHIM_WHISPER_MODEL path {model_path:?} is not valid UTF-8"
                ))
            })?;
            // Load the ggml model into a whisper-rs context (fails fast on a bad/missing model).
            let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
                .map_err(|error| boxed(format!("failed to load whisper model {path}: {error}")))?;
            let language = match language.trim().to_ascii_lowercase() {
                lang if lang.is_empty() || lang == "auto" => None,
                lang => Some(lang),
            };
            Ok(Self {
                ctx: Arc::new(ctx),
                language,
            })
        }

        /// Decode → detect language (+confidence) → transcribe. Blocking CPU work; the caller runs it
        /// on a blocking pool.
        fn run(&self, audio: Vec<u8>) -> Result<Transcript, DynError> {
            // 1. Decode the container to mono 16 kHz PCM (Ogg/Opus voice note, else via symphonia).
            let samples = decode_samples(audio)?;
            if samples.is_empty() {
                return Ok(Transcript::default());
            }

            let mut state = self
                .ctx
                .create_state()
                .map_err(|error| boxed(format!("whisper state init failed: {error}")))?;

            // 2. Language: pinned, or auto-detected with a confidence. `lang_detect` needs the mel
            //    spectrogram computed first (`pcm_to_mel`); best-effort, so a detection failure still
            //    transcribes (with `"auto"`) rather than dropping the clip.
            let (language, confidence) = match &self.language {
                Some(lang) => (Some(lang.clone()), None),
                None => {
                    let detected = state
                        .pcm_to_mel(&samples, 1)
                        .map_err(|error| boxed(format!("mel spectrogram failed: {error}")))
                        .and_then(|()| {
                            state
                                .lang_detect(0, 1)
                                .map_err(|error| boxed(format!("language detect failed: {error}")))
                        });
                    match detected {
                        Ok((id, probs)) => (
                            whisper_rs::get_lang_str(id).map(str::to_string),
                            probs.get(id as usize).copied(),
                        ),
                        Err(error) => {
                            tracing::warn!(%error, "whisper language auto-detect failed; transcribing without a language tag");
                            (None, None)
                        }
                    }
                }
            };

            // 3. Transcribe. Pin the language whisper should decode in (the detected/forced code, or
            //    "auto" so whisper detects internally when detection above was inconclusive).
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some(language.as_deref().unwrap_or("auto")));
            params.set_translate(false);
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            state
                .full(params, &samples)
                .map_err(|error| boxed(format!("whisper transcription failed: {error}")))?;

            let mut text = String::new();
            for segment in state.as_iter() {
                if let Ok(chunk) = segment.to_str() {
                    text.push_str(chunk);
                }
            }

            Ok(Transcript {
                text: text.trim().to_string(),
                language,
                language_confidence: confidence,
            })
        }
    }

    impl Transcriber for WhisperTranscriber {
        fn transcribe<'a>(
            &'a self,
            audio: &'a [u8],
            _mime: Option<&'a str>,
        ) -> TranscribeFuture<'a> {
            let ctx = self.ctx.clone();
            let language = self.language.clone();
            let audio = audio.to_vec();
            Box::pin(async move {
                let worker = WhisperTranscriber { ctx, language };
                tokio::task::spawn_blocking(move || worker.run(audio))
                    .await
                    .map_err(|error| boxed(format!("whisper worker panicked: {error}")))?
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_returns_canned_text() {
        let t = MockTranscriber::new("hello there");
        let out = t.transcribe(b"ignored", Some("audio/ogg")).await.unwrap();
        assert_eq!(out.text, "hello there");
        assert!(out.language.is_none());
    }

    /// Real end-to-end smoke test: decode → detect language → transcribe an actual clip with a real
    /// multilingual ggml model. Ignored by default (needs a model + audio the CI box doesn't have);
    /// run manually per `docs/TESTING.md` with:
    ///   `SMOKE_WHISPER_MODEL=/path/ggml-base.bin SMOKE_AUDIO=/path/clip.ogg \
    ///     cargo +1.95.0 test --features transcribe -- --ignored --nocapture smoke_transcribes`
    /// Assert nothing (content varies by clip) beyond a non-empty transcript; it prints the text +
    /// detected language + confidence so a human can eyeball a ≥2-language check.
    #[cfg(feature = "transcribe")]
    #[tokio::test]
    #[ignore = "needs SMOKE_WHISPER_MODEL + SMOKE_AUDIO (real model + clip)"]
    async fn smoke_transcribes_real_clip() {
        let model = std::env::var("SMOKE_WHISPER_MODEL").expect("set SMOKE_WHISPER_MODEL");
        let audio_path = std::env::var("SMOKE_AUDIO").expect("set SMOKE_AUDIO");
        let lang = std::env::var("SMOKE_LANG").unwrap_or_else(|_| "auto".into());
        let audio = std::fs::read(&audio_path).expect("read SMOKE_AUDIO");
        let transcriber = whisper::WhisperTranscriber::load(std::path::Path::new(&model), &lang)
            .expect("load model");
        let out = transcriber
            .transcribe(&audio, Some("audio/ogg"))
            .await
            .expect("transcribe");
        println!(
            "SMOKE transcript: {:?}\n  language={:?} confidence={:?}\n  body_block={:?}",
            out.text,
            out.language,
            out.language_confidence,
            out.body_block()
        );
        assert!(!out.text.trim().is_empty(), "expected non-empty transcript");
    }

    #[tokio::test]
    async fn noop_returns_empty() {
        let out = NoopTranscriber.transcribe(b"ignored", None).await.unwrap();
        assert_eq!(out, Transcript::default());
        assert!(out.body_block().is_none());
    }

    #[test]
    fn body_block_annotates_language_confidence_and_quotes_text() {
        // Language + confidence → `(es, 0.98)`.
        let with_conf = Transcript {
            text: "  hola mundo ".into(),
            language: Some("es".into()),
            language_confidence: Some(0.976),
        };
        assert_eq!(
            with_conf.body_block().as_deref(),
            Some("[voice note transcript (es, 0.98)]: \"hola mundo\"")
        );

        // Language, no confidence (pinned) → `(es)`.
        let pinned = Transcript {
            text: "hola".into(),
            language: Some("es".into()),
            language_confidence: None,
        };
        assert_eq!(
            pinned.body_block().as_deref(),
            Some("[voice note transcript (es)]: \"hola\"")
        );

        // No language → bare prefix.
        let no_lang = Transcript {
            text: "hello".into(),
            language: None,
            language_confidence: None,
        };
        assert_eq!(
            no_lang.body_block().as_deref(),
            Some("[voice note transcript]: \"hello\"")
        );

        // Blank language annotation is dropped; empty text yields no block at all.
        let blank_lang = Transcript {
            text: "hi".into(),
            language: Some("   ".into()),
            language_confidence: None,
        };
        assert_eq!(
            blank_lang.body_block().as_deref(),
            Some("[voice note transcript]: \"hi\"")
        );
        assert!(
            Transcript {
                text: "   ".into(),
                language: Some("en".into()),
                language_confidence: Some(0.9),
            }
            .body_block()
            .is_none()
        );
    }
}
