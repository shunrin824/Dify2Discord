// ---------------------------------------------------------------
// src/voice.rs
// 月見ヤチヨ ボイスチャット機能
//
// パイプライン:
//   Discord音声受信 (Opus, 48kHz Stereo)
//     → デコード済みPCM (VoiceTick)
//     → VAD (無音検出)
//     → 16kHz モノラル WAV 書き出し
//     → whisper-cli で音声認識 (STT)
//     → Dify チャット (テキストと同じ処理)
//     → VOICEVOX で音声合成 (TTS)
//     → VC で再生
// ---------------------------------------------------------------
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Local;
use reqwest::Client as HttpClient;
use serde_json::json;
use serenity::model::id::UserId;
use songbird::{
    model::payload::Speaking, Call, CoreEvent, Event, EventContext,
    EventHandler as VoiceEventHandler,
};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::{add_segment_to_dify, append_chat_log, consume_dify_stream, strip_think_tags};

// ---------------------------------------------------------------
// 定数
// ---------------------------------------------------------------

/// 無音と判断するフレーム数（20ms × 25 = 500ms）
const SILENCE_THRESHOLD: u32 = 25;

/// 処理に必要な最低発話フレーム数（20ms × 10 = 200ms）
/// これ未満の発話は短すぎるとしてスキップする
const MIN_SPEECH_FRAMES: u32 = 10;

// ---------------------------------------------------------------
// ユーザーごとの音声バッファ
// ---------------------------------------------------------------
struct UserBuffer {
    /// 48kHz ステレオ i16 PCM の蓄積バッファ
    pcm: Vec<i16>,
    /// 直近の無音フレーム数（発話後のみカウント）
    silence_count: u32,
    /// 蓄積した発話フレーム数
    speech_frames: u32,
}

impl Default for UserBuffer {
    fn default() -> Self {
        Self {
            pcm: Vec::new(),
            silence_count: 0,
            speech_frames: 0,
        }
    }
}

// ---------------------------------------------------------------
// ボイスハンドラー間で共有する状態
// ---------------------------------------------------------------
pub struct VoiceShared {
    pub db: SqlitePool,
    pub http_client: HttpClient,
    pub memory_log_path: String,
    /// songbird の Call (VC 操作・再生に使用)
    pub call: Arc<Mutex<Call>>,
    /// VOICEVOX の speaker_id（冥鳴ひまり）
    pub speaker_id: u32,

    /// SSRC → Discord UserId マッピング
    ssrc_map: Mutex<HashMap<u32, UserId>>,
    /// SSRC ごとの音声バッファ
    buffers: Mutex<HashMap<u32, UserBuffer>>,
    /// 現在 STT 処理中の SSRC セット（二重処理防止）
    processing: Mutex<HashSet<u32>>,
}

impl VoiceShared {
    pub fn new(
        db: SqlitePool,
        http_client: HttpClient,
        memory_log_path: String,
        call: Arc<Mutex<Call>>,
        speaker_id: u32,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            http_client,
            memory_log_path,
            call,
            speaker_id,
            ssrc_map: Mutex::new(HashMap::new()),
            buffers: Mutex::new(HashMap::new()),
            processing: Mutex::new(HashSet::new()),
        })
    }
}

// ---------------------------------------------------------------
// SpeakingStateUpdate ハンドラー
// Discord が送ってくる SSRC → UserId マッピングを記録する
// ---------------------------------------------------------------
pub struct SpeakingStateHandler(pub Arc<VoiceShared>);

#[async_trait]
impl VoiceEventHandler for SpeakingStateHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::SpeakingStateUpdate(Speaking {
            ssrc, user_id, ..
        }) = ctx
        {
            if let Some(uid) = user_id {
                self.0
                    .ssrc_map
                    .lock()
                    .await
                    .insert(*ssrc, UserId::new(uid.get()));
                println!("[VOICE] SSRC {} → UserId {}", ssrc, uid.get());
            }
        }
        None
    }
}

// ---------------------------------------------------------------
// VoiceTick ハンドラー
// 20ms ごとに呼ばれ、ユーザーの音声を蓄積して VAD を行う
// ---------------------------------------------------------------
pub struct VoiceTickHandler(pub Arc<VoiceShared>);

#[async_trait]
impl VoiceEventHandler for VoiceTickHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::VoiceTick(tick) = ctx else {
            return None;
        };

        let mut buffers = self.0.buffers.lock().await;
        let mut to_process: Vec<(u32, Vec<i16>)> = Vec::new();

        // ── 発話中のユーザーのPCMを蓄積 ─────────────────────────
        for (&ssrc, voice_data) in &tick.speaking {
            let buf = buffers.entry(ssrc).or_default();

            if let Some(decoded) = voice_data.decoded_voice.as_deref() {
                // 音声データあり: バッファに追加、無音カウントリセット
                buf.pcm.extend_from_slice(decoded);
                buf.silence_count = 0;
                buf.speech_frames += 1;
            } else if buf.speech_frames > 0 {
                // RTPパケットはあるが無音フレーム
                buf.silence_count += 1;
            }
        }

        // ── 無音ユーザーの無音カウントを進める ────────────────────
        for &ssrc in &tick.silent {
            if let Some(buf) = buffers.get_mut(&ssrc) {
                if buf.speech_frames > 0 {
                    buf.silence_count += 1;
                }
            }
        }

        // ── VAD: 無音閾値を超えたら処理キューへ ──────────────────
        {
            let proc = self.0.processing.lock().await;
            for (&ssrc, buf) in buffers.iter_mut() {
                if buf.silence_count >= SILENCE_THRESHOLD
                    && buf.speech_frames >= MIN_SPEECH_FRAMES
                    && !proc.contains(&ssrc)
                {
                    // バッファを取り出して処理対象に
                    to_process.push((ssrc, std::mem::take(&mut buf.pcm)));
                    buf.silence_count = 0;
                    buf.speech_frames = 0;
                }
            }
        }
        drop(buffers);

        // ── 非同期でSTT→Dify→TTS→再生パイプラインを起動 ─────────
        for (ssrc, pcm) in to_process {
            let shared = self.0.clone();
            tokio::spawn(async move {
                shared.processing.lock().await.insert(ssrc);

                if let Err(e) = process_voice(shared.clone(), ssrc, pcm).await {
                    eprintln!("[VOICE ERROR] ssrc={}: {}", ssrc, e);
                }

                shared.processing.lock().await.remove(&ssrc);
            });
        }

        None
    }
}

// ---------------------------------------------------------------
// 音声処理パイプライン本体
// ---------------------------------------------------------------
async fn process_voice(
    shared: Arc<VoiceShared>,
    ssrc: u32,
    pcm_48k_stereo: Vec<i16>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ユーザーIDを取得（SSRC マッピングから）
    let user_label = {
        let map = shared.ssrc_map.lock().await;
        map.get(&ssrc)
            .map(|u| u.to_string())
            .unwrap_or_else(|| format!("ssrc_{}", ssrc))
    };

    let duration_sec = pcm_48k_stereo.len() as f32 / (48000.0 * 2.0);
    println!(
        "[VOICE] {} の音声を処理するよ〜 ({:.1}秒)",
        user_label, duration_sec
    );

    // ── ① 48kHz ステレオ → 16kHz モノラル ───────────────────────
    let pcm_16k_mono = downsample_stereo_48k_to_mono_16k(&pcm_48k_stereo);

    // ── ② WAV ファイルに書き出す ─────────────────────────────────
    let wav_in_path = format!("/tmp/yachiyo_in_{}.wav", ssrc);
    if let Err(e) = write_wav_16k_mono(&wav_in_path, &pcm_16k_mono) {
        return Err(format!("WAV書き出し失敗: {}", e).into());
    }

    // ── ③ whisper-cli で音声認識 ─────────────────────────────────
    let stt_result = run_whisper(&wav_in_path).await;
    let _ = tokio::fs::remove_file(&wav_in_path).await;

    let text = stt_result?.trim().to_string();

    if text.is_empty() || is_noise_or_hallucination(&text) {
        println!("[VOICE] {} 無音/ノイズ判定、スキップするよ〜 (\"{}\")", user_label, text);
        return Ok(());
    }
    println!("[VOICE] {} → STT結果: \"{}\"", user_label, text);

    // ── ④ Dify チャット（テキストチャンネルと同じ処理）─────────
    let dify_api_key = env::var("DIFY_API_KEY").unwrap_or_default();
    let dify_base_url =
        env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());

    // VC ユーザーごとに会話IDを保持する
    let voice_key = format!("voice_{}", user_label);
    let conv_id: String =
        sqlx::query_scalar("SELECT conversation_id FROM conversations WHERE key = ?")
            .bind(&voice_key)
            .fetch_optional(&shared.db)
            .await
            .unwrap_or(None)
            .unwrap_or_default();

    let current_time = Local::now().format("%Y年%m月%d日 %H:%M").to_string();
    let payload = json!({
        "inputs": {"current_time": current_time},
        "query": text,
        "response_mode": "streaming",
        "conversation_id": conv_id,
        "user": user_label,
    });

    let dify_res = shared
        .http_client
        .post(format!("{}/v1/chat-messages", dify_base_url))
        .header("Authorization", format!("Bearer {}", dify_api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !dify_res.status().is_success() {
        return Err(format!("Dify HTTP {}", dify_res.status()).into());
    }

    let dify_result = consume_dify_stream(dify_res).await?;

    // 会話ID を DB に保存
    if !dify_result.conversation_id.is_empty() {
        sqlx::query(
            "INSERT INTO conversations (key, conversation_id)
             VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET conversation_id = excluded.conversation_id",
        )
        .bind(&voice_key)
        .bind(&dify_result.conversation_id)
        .execute(&shared.db)
        .await
        .ok();
    }

    let display_answer = strip_think_tags(&dify_result.answer);
    if display_answer.is_empty() {
        eprintln!("[VOICE WARN] {} think除去後の回答が空", user_label);
        return Ok(());
    }
    println!("[VOICE] ヤチヨの返答: \"{}\"", display_answer);

    // ── ⑤ ログ・ナレッジ保存（バックグラウンド）────────────────
    {
        let hc = shared.http_client.clone();
        let mlp = shared.memory_log_path.clone();
        let q = text.clone();
        let a = dify_result.answer.clone();
        tokio::spawn(async move {
            if let Err(e) = append_chat_log(&mlp, &q, &a).await {
                eprintln!("[VOICE WARN] ログ保存失敗: {}", e);
            }
            if let Err(e) = add_segment_to_dify(&hc, &q, &a).await {
                eprintln!("[VOICE WARN] Difyセグメント追加失敗: {}", e);
            }
        });
    }

    // ── ⑥ VOICEVOX で音声合成 ────────────────────────────────────
    let wav_bytes = voicevox_synthesize(&shared.http_client, &display_answer, shared.speaker_id)
        .await
        .map_err(|e| format!("VOICEVOX合成失敗: {}", e))?;

    // ── ⑦ VC で再生 ──────────────────────────────────────────────
    let wav_out_path = format!(
        "/tmp/yachiyo_out_{}.wav",
        Local::now().timestamp_millis()
    );
    tokio::fs::write(&wav_out_path, &wav_bytes).await?;

    {
        let mut call = shared.call.lock().await;
        let src = songbird::input::File::new(wav_out_path.clone());
        call.play_input(src.into());
    }

    // 再生完了後にファイルを削除（最大30秒待つ）
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        let _ = tokio::fs::remove_file(&wav_out_path).await;
    });

    Ok(())
}

// ---------------------------------------------------------------
// 48kHz ステレオ i16 → 16kHz モノラル i16 変換
//
// 1. L+R の平均でモノラル化 (48kHz)
// 2. 3サンプルに1つ取り出してデシメーション (48000/3 = 16000)
// ---------------------------------------------------------------
fn downsample_stereo_48k_to_mono_16k(stereo: &[i16]) -> Vec<i16> {
    // ステレオ → モノラル（L+R 平均）
    let mono_48k: Vec<i16> = stereo
        .chunks_exact(2)
        .map(|lr| ((lr[0] as i32 + lr[1] as i32) / 2) as i16)
        .collect();

    // デシメーション (48kHz → 16kHz): 3サンプルの平均を1サンプルに
    mono_48k
        .chunks(3)
        .map(|chunk| {
            let sum: i32 = chunk.iter().map(|&s| s as i32).sum();
            (sum / chunk.len() as i32) as i16
        })
        .collect()
}

// ---------------------------------------------------------------
// 16kHz モノラル i16 を WAV ファイルに書き出す
// ---------------------------------------------------------------
fn write_wav_16k_mono(
    path: &str,
    samples: &[i16],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}

// ---------------------------------------------------------------
// whisper-cli で音声認識を行い、認識テキストを返す
//
// .env:
//   WHISPER_BIN   = whisper-cli の実行ファイルパス
//   WHISPER_MODEL = モデルファイルのパス (ggml-*.bin)
// ---------------------------------------------------------------
async fn run_whisper(
    wav_path: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let bin = env::var("WHISPER_BIN").unwrap_or_else(|_| "whisper-cli".to_string());
    let model = env::var("WHISPER_MODEL")
        .unwrap_or_else(|_| "/usr/local/share/whisper/ggml-base.bin".to_string());

    let output = tokio::process::Command::new(&bin)
        .args([
            "-m", &model,
            "-f", wav_path,
            "-l", "ja",
            "-nt",  // no timestamps
            "-np",  // no progress bar
        ])
        .output()
        .await
        .map_err(|e| format!("whisper-cli 実行失敗: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("whisper-cli エラー: {}", stderr).into());
    }

    // 標準出力からタイムスタンプ行や空行を除いてテキストを抽出
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // "[hh:mm:ss.xxx --> ...]" 形式のタイムスタンプ行を除外
        .filter(|l| !l.starts_with('['))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(text)
}

// ---------------------------------------------------------------
// ノイズや幻覚（hallucination）の出力かを判定する
// ---------------------------------------------------------------
fn is_noise_or_hallucination(text: &str) -> bool {
    let t = text.trim();
    if t.len() < 2 {
        return true;
    }
    // whisper が無音時に出力しがちなパターン
    let noise_patterns = [
        "[BLANK_AUDIO]",
        "(無音)",
        "(silence)",
        "ご視聴ありがとうございました",
        "字幕は自動生成されました",
        "チャンネル登録",
        "ありがとうございました。",
    ];
    noise_patterns.iter().any(|p| t.contains(p))
}

// ---------------------------------------------------------------
// VOICEVOX で音声合成して WAV バイト列を返す
//
// .env:
//   VOICEVOX_URL        = VOICEVOX エンジンの URL（デフォルト: http://localhost:50021）
//   VOICEVOX_SPEAKER_ID = speaker_id を上書きしたい場合（省略で自動検出）
// ---------------------------------------------------------------
async fn voicevox_synthesize(
    client: &HttpClient,
    text: &str,
    speaker_id: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let base_url =
        env::var("VOICEVOX_URL").unwrap_or_else(|_| "http://localhost:50021".to_string());

    // Step 1: audio_query の生成
    let query_res = client
        .post(format!("{}/audio_query", base_url))
        .query(&[("text", text), ("speaker", &speaker_id.to_string())])
        .header("Content-Type", "application/json")
        .send()
        .await?;

    if !query_res.status().is_success() {
        let status = query_res.status();
        let body = query_res.text().await.unwrap_or_default();
        return Err(format!("VOICEVOX audio_query 失敗: {} / {}", status, body).into());
    }

    let query_json: serde_json::Value = query_res.json().await?;

    // Step 2: 音声合成
    let synth_res = client
        .post(format!("{}/synthesis", base_url))
        .query(&[("speaker", speaker_id.to_string())])
        .header("Content-Type", "application/json")
        .json(&query_json)
        .send()
        .await?;

    if !synth_res.status().is_success() {
        let status = synth_res.status();
        let body = synth_res.text().await.unwrap_or_default();
        return Err(format!("VOICEVOX synthesis 失敗: {} / {}", status, body).into());
    }

    let wav_bytes = synth_res.bytes().await?.to_vec();
    Ok(wav_bytes)
}

// ---------------------------------------------------------------
// 起動時: VOICEVOX から冥鳴ひまりの speaker_id を自動取得する
//
// 見つからない場合は VOICEVOX_SPEAKER_ID 環境変数（省略時: 75）を使う
// ---------------------------------------------------------------
pub async fn resolve_himari_speaker_id(client: &HttpClient) -> u32 {
    let base_url =
        env::var("VOICEVOX_URL").unwrap_or_else(|_| "http://localhost:50021".to_string());

    // .env で明示指定があればそちらを優先
    let fallback: u32 = env::var("VOICEVOX_SPEAKER_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(75); // 冥鳴ひまり ノーマルの一般的なデフォルト値

    let speakers_res = match client
        .get(format!("{}/speakers", base_url))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            eprintln!(
                "[WARN] VOICEVOX /speakers が {} を返したよ。speaker_id={} を使うよ〜",
                r.status(), fallback
            );
            return fallback;
        }
        Err(e) => {
            eprintln!(
                "[WARN] VOICEVOX への接続に失敗したよ: {}。speaker_id={} を使うよ〜",
                e, fallback
            );
            return fallback;
        }
    };

    let speakers: serde_json::Value = match speakers_res.json().await {
        Ok(v) => v,
        Err(_) => return fallback,
    };

    let Some(arr) = speakers.as_array() else {
        return fallback;
    };

    for speaker in arr {
        if speaker["name"].as_str() == Some("冥鳴ひまり") {
            // スタイル一覧の最初（ノーマル）の id を使う
            if let Some(style_id) = speaker["styles"][0]["id"].as_u64() {
                println!("[VOICE] 冥鳴ひまり を発見！ speaker_id = {}", style_id);
                return style_id as u32;
            }
        }
    }

    eprintln!(
        "[WARN] 冥鳴ひまりが VOICEVOX のスピーカー一覧に見つからなかったよ。speaker_id={} を使うよ〜",
        fallback
    );
    fallback
}
