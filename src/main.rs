mod web;

use serenity::async_trait;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use serde_json::json;
use reqwest::Client as HttpClient;
use std::env;
use futures_util::StreamExt;
use sqlx::SqlitePool;
use chrono::Local;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

// ---------------------------------------------------------------
// <think>〜</think> ブロックをDiscord表示用に除去する
// ログ・ナレッジベースへの書き込みには元のテキストを使用すること
// ---------------------------------------------------------------
pub fn strip_think_tags(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;
    loop {
        match remaining.find("<think>") {
            None => {
                result.push_str(remaining);
                break;
            }
            Some(start) => {
                result.push_str(&remaining[..start]);
                remaining = &remaining[start + "<think>".len()..];
                match remaining.find("</think>") {
                    None => break,
                    Some(end) => {
                        remaining = &remaining[end + "</think>".len()..];
                    }
                }
            }
        }
    }
    result.trim().to_string()
}

// ---------------------------------------------------------------
// チャットログをNASのMarkdownファイルに非同期追記する
// ---------------------------------------------------------------
pub async fn append_chat_log(
    memory_log_path: &str,
    user_input: &str,
    bot_reply: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let entry = format!(
        "\n## [{}] の会話\n- shunrin824: {}\n- 月見ヤチヨ: {}\n",
        timestamp, user_input, bot_reply
    );
    if let Some(parent) = std::path::Path::new(memory_log_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(memory_log_path)
        .await?;
    file.write_all(entry.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------
// Difyナレッジベースへリアルタイムでセグメントを追加する
// ---------------------------------------------------------------
pub async fn add_segment_to_dify(
    http_client: &HttpClient,
    user_input: &str,
    bot_reply: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dify_base_url =
        env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
    let dataset_api_key =
        env::var("DIFY_DATASET_API_KEY").map_err(|_| "DIFY_DATASET_API_KEYが設定されていないよ！")?;
    let dataset_id =
        env::var("DIFY_DATASET_ID").map_err(|_| "DIFY_DATASET_IDが設定されていないよ！")?;
    let document_id =
        env::var("DIFY_DOCUMENT_ID").map_err(|_| "DIFY_DOCUMENT_IDが設定されていないよ！")?;

    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let content = format!(
        "[{}] の会話\nshunrin824: {}\n月見ヤチヨ: {}",
        timestamp, user_input, bot_reply
    );

    let url = format!(
        "{}/v1/datasets/{}/documents/{}/segments",
        dify_base_url, dataset_id, document_id
    );

    let payload = json!({
        "segments": [{"content": content, "keywords": []}]
    });

    println!("[DEBUG] セグメント追加先 URL: {}", url);
    println!("[DEBUG] dataset_id:  {}", dataset_id);
    println!("[DEBUG] document_id: {}", document_id);
    println!("[DEBUG] 書き込み内容: {}", content);

    let res = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", dataset_api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    println!("[DEBUG] セグメント追加レスポンス: {} / {}", status, body);

    if !status.is_success() {
        return Err(format!("Difyセグメント追加エラー: {} / {}", status, body).into());
    }
    Ok(())
}

// ---------------------------------------------------------------
// 起動時: ナレッジベースとドキュメントを自動作成して.envに書き出す
// ---------------------------------------------------------------
pub async fn setup_dify_knowledge(http_client: &HttpClient) {
    if env::var("DIFY_DATASET_ID").is_ok() && env::var("DIFY_DOCUMENT_ID").is_ok() {
        println!("ナレッジベース設定済み、セットアップをスキップするよ〜");
        return;
    }

    let dify_base_url =
        env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
    let dataset_api_key =
        env::var("DIFY_DATASET_API_KEY").expect("DIFY_DATASET_API_KEYが設定されていないよ！");

    let dataset_res = http_client
        .post(format!("{}/v1/datasets", dify_base_url))
        .header("Authorization", format!("Bearer {}", dataset_api_key))
        .header("Content-Type", "application/json")
        .json(&json!({
            "name": "ヤチヨの長期記憶",
            "description": "月見ヤチヨとshunrin824の会話ログ",
            "indexing_technique": "high_quality",
            "permission": "only_me",
            "embedding_model": "bge-m3",
            "embedding_model_provider": "ollama"
        }))
        .send()
        .await;

    let dataset_id = match dataset_res {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.expect("レスポンスのパースに失敗");
            body["id"]
                .as_str()
                .expect("dataset_idが取れなかった")
                .to_string()
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            panic!("ナレッジベース作成失敗: {} / {}", status, body);
        }
        Err(e) => panic!("ナレッジベース作成エラー: {:?}", e),
    };
    println!("ナレッジベース作成完了！ dataset_id: {}", dataset_id);

    let doc_res = http_client
        .post(format!(
            "{}/v1/datasets/{}/document/create_by_text",
            dify_base_url, dataset_id
        ))
        .header("Authorization", format!("Bearer {}", dataset_api_key))
        .header("Content-Type", "application/json")
        .json(&json!({
            "name": "chat_memory",
            "text": "# 月見ヤチヨの長期記憶\nこのドキュメントにはヤチヨとshunrin824の会話ログが蓄積されます。\n",
            "indexing_technique": "high_quality",
            "embedding_model": "bge-m3",
            "embedding_model_provider": "ollama",
            "process_rule": {"mode": "automatic"}
        }))
        .send()
        .await;

    let document_id = match doc_res {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.expect("レスポンスのパースに失敗");
            body["document"]["id"]
                .as_str()
                .expect("document_idが取れなかった")
                .to_string()
        }
        Ok(r) => panic!("ドキュメント作成失敗: {}", r.status()),
        Err(e) => panic!("ドキュメント作成エラー: {:?}", e),
    };
    println!("ドキュメント作成完了！ document_id: {}", document_id);

    let env_addition = format!(
        "\nDIFY_DATASET_ID={}\nDIFY_DOCUMENT_ID={}\n",
        dataset_id, document_id
    );
    if let Ok(mut f) = OpenOptions::new().append(true).open(".env").await {
        let _ = f.write_all(env_addition.as_bytes()).await;
        println!(".envにIDを書き込んだよ〜！");
    }
    unsafe {
        env::set_var("DIFY_DATASET_ID", &dataset_id);
        env::set_var("DIFY_DOCUMENT_ID", &document_id);
    }
}

// ---------------------------------------------------------------
// 画像フォーマット判定（pub: web.rs からも使用）
// ---------------------------------------------------------------
pub fn resolve_image_type(content_type: &str, filename: &str) -> Option<&'static str> {
    let lower_ct = content_type.to_lowercase();
    let lower_fn = filename.to_lowercase();
    if lower_ct.contains("jpeg") || lower_ct.contains("jpg")
        || lower_fn.ends_with(".jpg") || lower_fn.ends_with(".jpeg")
    {
        Some("jpg")
    } else if lower_ct.contains("png") || lower_fn.ends_with(".png") {
        Some("png")
    } else if lower_ct.contains("webp") || lower_fn.ends_with(".webp") {
        Some("webp")
    } else {
        None
    }
}

async fn convert_to_jpeg_q100(
    raw_bytes: Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&raw_bytes)?;
        let mut jpeg_buf: Vec<u8> = Vec::new();
        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, 100);
        encoder.encode_image(&img)?;
        Ok(jpeg_buf)
    })
    .await?
}

async fn process_single_attachment(
    http_client: &HttpClient,
    attachment_url: &str,
    original_filename: &str,
    content_type: &str,
    dify_api_key: &str,
    dify_base_url: &str,
    image_save_dir: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let raw_bytes = http_client
        .get(attachment_url)
        .send()
        .await?
        .bytes()
        .await?
        .to_vec();

    let ext = resolve_image_type(content_type, original_filename).ok_or_else(|| {
        format!(
            "非対応の画像フォーマット: {} / {}",
            content_type, original_filename
        )
    })?;

    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
    let stem = std::path::Path::new(original_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let save_filename = format!("{}_{}.{}", timestamp, stem, ext);
    let save_path = format!(
        "{}/{}",
        image_save_dir.trim_end_matches('/'),
        save_filename
    );

    tokio::fs::create_dir_all(image_save_dir).await?;
    tokio::fs::write(&save_path, &raw_bytes).await?;
    println!("[画像保存] {}", save_path);

    let jpeg_bytes = convert_to_jpeg_q100(raw_bytes).await?;
    let upload_filename = format!("{}_{}.jpg", timestamp, stem);

    let part = reqwest::multipart::Part::bytes(jpeg_bytes)
        .file_name(upload_filename)
        .mime_str("image/jpeg")?;
    let form = reqwest::multipart::Form::new()
        .text("type", "image")
        .part("file", part);

    let upload_res = http_client
        .post(format!("{}/v1/files/upload", dify_base_url))
        .header("Authorization", format!("Bearer {}", dify_api_key))
        .multipart(form)
        .send()
        .await?;

    if !upload_res.status().is_success() {
        let status = upload_res.status();
        let body = upload_res.text().await.unwrap_or_default();
        return Err(format!("Difyファイルアップロード失敗: {} / {}", status, body).into());
    }

    let upload_body: serde_json::Value = upload_res.json().await?;
    let upload_file_id = upload_body["id"]
        .as_str()
        .ok_or("upload_file_idが取得できなかった")?
        .to_string();

    println!("[Difyアップロード] upload_file_id: {}", upload_file_id);
    Ok(upload_file_id)
}

// ---------------------------------------------------------------
// Dify SSEストリームのパース
// ---------------------------------------------------------------
#[derive(Debug)]
pub struct DifyStreamResult {
    pub answer: String,
    pub conversation_id: String,
}

pub async fn consume_dify_stream(
    response: reqwest::Response,
) -> Result<DifyStreamResult, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = response.bytes_stream();
    let mut full_answer = String::new();
    let mut conversation_id = String::new();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        let lines: Vec<&str> = buffer.lines().collect();
        for line in &lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let json_str = if let Some(s) = line.strip_prefix("data: ") {
                s
            } else {
                continue;
            };

            if json_str == "[DONE]" {
                break;
            }

            match serde_json::from_str::<serde_json::Value>(json_str) {
                Ok(body) => {
                    if body["event"].as_str() == Some("error") {
                        let code = body["code"].as_str().unwrap_or("unknown");
                        let message = body["message"].as_str().unwrap_or("unknown error");
                        return Err(
                            format!("Dify error event: code={} message={}", code, message)
                                .into(),
                        );
                    }

                    if let Some(answer) = body["answer"].as_str() {
                        full_answer.push_str(answer);
                    }
                    if let Some(cid) = body["conversation_id"].as_str() {
                        if !cid.is_empty() {
                            conversation_id = cid.to_string();
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[WARN] SSEパース失敗: {} / 行: {}", e, json_str);
                }
            }
        }

        if buffer.ends_with('\n') {
            buffer.clear();
        } else if let Some(last_nl) = buffer.rfind('\n') {
            buffer = buffer[last_nl + 1..].to_string();
        }
    }

    Ok(DifyStreamResult {
        answer: full_answer,
        conversation_id,
    })
}

// ---------------------------------------------------------------
// Discord Event Handler
// ---------------------------------------------------------------
struct Handler {
    db: SqlitePool,
    http_client: HttpClient,
    vision_enabled: bool,
    memory_log_path: String,
    image_save_dir: String,
    admin_user_ids: Vec<String>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        // ---------------------------------------------------------------
        // 管理者コマンド: !ollama-restart
        // ---------------------------------------------------------------
        if msg.content.trim() == "!ollama-restart" {
            let author_id = msg.author.id.to_string();

            if !self.admin_user_ids.contains(&author_id) {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "ごめんね、このコマンドは管理者しか使えないよ〜。")
                    .await;
                return;
            }

            let _ = msg
                .channel_id
                .say(&ctx.http, "Ollamaを再起動するよ〜、少し待ってね！")
                .await;

            match tokio::process::Command::new("systemctl")
                .args(["restart", "ollama"])
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    println!("[INFO] ollama restart 成功");
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, "Ollamaの再起動が完了したよ〜！")
                        .await;
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    eprintln!("[ERROR] ollama restart 失敗: {}", stderr);
                    let _ = msg
                        .channel_id
                        .say(
                            &ctx.http,
                            format!("再起動に失敗しちゃったみたい〜。\n```\n{}\n```", stderr),
                        )
                        .await;
                }
                Err(e) => {
                    eprintln!("[ERROR] systemctl実行エラー: {:?}", e);
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, "systemctlの実行自体に失敗しちゃったよ〜。")
                        .await;
                }
            }
            return;
        }

        let dify_api_key =
            env::var("DIFY_API_KEY").expect("DIFY_API_KEYが設定されていないよ！");
        let dify_base_url =
            env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
        let dify_chat_url = format!("{}/v1/chat-messages", dify_base_url);

        let user_id = msg.author.id.to_string();
        let channel_id = msg.channel_id.to_string();
        let key = format!("{}_{}", user_id, channel_id);

        let _ = msg.channel_id.broadcast_typing(&ctx.http).await;

        let conv_id: String = sqlx::query_scalar(
            "SELECT conversation_id FROM conversations WHERE key = ?",
        )
        .bind(&key)
        .fetch_optional(&self.db)
        .await
        .unwrap_or(None)
        .unwrap_or_default();

        // ================================================================
        // ルートA: 添付画像の処理
        // ================================================================
        let mut dify_files: Vec<serde_json::Value> = Vec::new();

        if self.vision_enabled {
            for attachment in &msg.attachments {
                let ct = attachment.content_type.clone().unwrap_or_default();

                if resolve_image_type(&ct, &attachment.filename).is_none() {
                    println!(
                        "[スキップ] 非対応フォーマット: {} ({})",
                        attachment.filename, ct
                    );
                    continue;
                }

                match process_single_attachment(
                    &self.http_client,
                    &attachment.url,
                    &attachment.filename,
                    &ct,
                    &dify_api_key,
                    &dify_base_url,
                    &self.image_save_dir,
                )
                .await
                {
                    Ok(upload_file_id) => {
                        dify_files.push(json!({
                            "type": "image",
                            "transfer_method": "local_file",
                            "upload_file_id": upload_file_id
                        }));
                    }
                    Err(e) => {
                        eprintln!("[WARN] 画像処理失敗 ({}): {}", attachment.filename, e);
                    }
                }
            }
        } else if !msg.attachments.is_empty() {
            println!(
                "[INFO] vision無効のため{}枚の画像をスキップ",
                msg.attachments.len()
            );
        }

        // ================================================================
        // Dify chat-messages へ送信
        // ================================================================
        let current_time = Local::now().format("%Y年%m月%d日 %H:%M").to_string();

        let query_text = if msg.content.is_empty() && !msg.attachments.is_empty() {
            "（画像を送ったよ）".to_string()
        } else {
            msg.content.clone()
        };

        let mut payload = json!({
            "inputs": {"current_time": current_time},
            "query": query_text,
            "response_mode": "streaming",
            "conversation_id": conv_id,
            "user": user_id
        });

        if !dify_files.is_empty() {
            payload["files"] = json!(dify_files);
        }

        let res = self
            .http_client
            .post(&dify_chat_url)
            .header("Authorization", format!("Bearer {}", dify_api_key))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await;

        match res {
            Err(e) => {
                eprintln!("[ERROR] Difyへの接続エラー: {:?}", e);
                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        "ごめんね、今ちょっと頭が回らなくてお返事できないみたい〜。",
                    )
                    .await;
                return;
            }
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    eprintln!("[ERROR] Dify HTTP {}: {}", status, body);
                    let _ = msg
                        .channel_id
                        .say(
                            &ctx.http,
                            format!(
                                "ごめんね、ヤッチョのサーバー側でエラーが出ちゃったみたい〜。（{}）",
                                status
                            ),
                        )
                        .await;
                    return;
                }

                match consume_dify_stream(response).await {
                    Err(e) => {
                        eprintln!("[ERROR] ストリームのパースエラー: {:?}", e);
                        let _ = msg
                            .channel_id
                            .say(
                                &ctx.http,
                                "ごめんね、返事の受け取りに失敗しちゃったみたい〜。",
                            )
                            .await;
                    }
                    Ok(result) => {
                        if !result.conversation_id.is_empty() {
                            sqlx::query(
                                "INSERT INTO conversations (key, conversation_id)
                                 VALUES (?, ?)
                                 ON CONFLICT(key) DO UPDATE SET conversation_id = excluded.conversation_id",
                            )
                            .bind(&key)
                            .bind(&result.conversation_id)
                            .execute(&self.db)
                            .await
                            .ok();
                        }

                        if result.answer.is_empty() {
                            eprintln!("[WARN] Difyからの回答が空");
                            let _ = msg
                                .channel_id
                                .say(
                                    &ctx.http,
                                    "ごめんね、うまく返事が受け取れなかったみたい〜。",
                                )
                                .await;
                            return;
                        }

                        let display_answer = strip_think_tags(&result.answer);

                        if display_answer.is_empty() {
                            eprintln!("[WARN] think除去後の表示テキストが空（think onlyの応答）");
                            let _ = msg
                                .channel_id
                                .say(
                                    &ctx.http,
                                    "ごめんね、うまく返事が受け取れなかったみたい〜。",
                                )
                                .await;
                            return;
                        }

                        for chunk in split_message(&display_answer, 1900) {
                            let _ = msg.channel_id.say(&ctx.http, &chunk).await;
                        }

                        let http_client = self.http_client.clone();
                        let memory_log_path = self.memory_log_path.clone();
                        let user_input = query_text.clone();
                        let bot_reply = result.answer.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                append_chat_log(&memory_log_path, &user_input, &bot_reply).await
                            {
                                eprintln!("[WARN] ログ保存失敗: {}", e);
                            }
                            if let Err(e) =
                                add_segment_to_dify(&http_client, &user_input, &bot_reply)
                                    .await
                            {
                                eprintln!("[WARN] Difyセグメント追加失敗: {}", e);
                            }
                        });
                    }
                }
            }
        }
    }

    async fn ready(&self, _: Context, ready: Ready) {
        println!(
            "ヤチヨ、Discordにログイン完了したよ〜！ ({})",
            ready.user.name
        );
    }
}

// ---------------------------------------------------------------
// Discordの2000文字制限に対応した分割
// ---------------------------------------------------------------
fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.split('\n') {
        if line.len() > limit {
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }
            let chars: Vec<char> = line.chars().collect();
            for c in chars.chunks(limit) {
                chunks.push(c.iter().collect());
            }
            continue;
        }

        if current.len() + line.len() + 1 > limit {
            chunks.push(current.clone());
            current.clear();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

// ---------------------------------------------------------------
// エントリポイント
// ---------------------------------------------------------------
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    let discord_token =
        env::var("DISCORD_TOKEN").expect("DISCORD_TOKENが設定されていないよ！");

    let memory_log_path = env::var("MEMORY_LOG_PATH")
        .unwrap_or_else(|_| "/home/shunrin824/data16_1/yachiyo/chat_logs/memory.md".to_string());
    let image_save_dir = env::var("IMAGE_SAVE_DIR")
        .unwrap_or_else(|_| "/home/shunrin824/data16_1/yachiyo/images/".to_string());

    let admin_user_ids: Vec<String> = env::var("ADMIN_USER_IDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    println!("[設定] MEMORY_LOG_PATH: {}", memory_log_path);
    println!("[設定] IMAGE_SAVE_DIR:   {}", image_save_dir);
    if admin_user_ids.is_empty() {
        println!("[WARN] ADMIN_USER_IDSが未設定です。!ollama-restartは使用できません。");
    } else {
        println!("[設定] 管理者ユーザー数: {}人", admin_user_ids.len());
    }

    let db = SqlitePool::connect("sqlite:conversations.db?mode=rwc")
        .await
        .expect("DBの接続に失敗しちゃった");

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS conversations (
            key TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL
        )",
    )
    .execute(&db)
    .await
    .expect("テーブルの作成に失敗しちゃった");

    let http_client = HttpClient::new();

    setup_dify_knowledge(&http_client).await;

    let vision_enabled = env::var("VISION_ENABLED")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    println!(
        "vision機能: {}",
        if vision_enabled { "有効" } else { "無効" }
    );

    // ---------------------------------------------------------------
    // Basic認証設定の読み込み
    // ---------------------------------------------------------------
    let auth_user = env::var("WEB_AUTH_USER").ok().filter(|s| !s.is_empty());
    let auth_pass = env::var("WEB_AUTH_PASS").ok();
    if auth_user.is_some() {
        println!("[設定] Webインターフェイス: Basic認証が有効です");
    } else {
        println!("[WARN] WEB_AUTH_USERが未設定です。Webインターフェイスは認証なしで公開されます。");
    }

    // ---------------------------------------------------------------
    // TLS設定の読み込み
    // ---------------------------------------------------------------
    let tls_enabled = env::var("TLS_ENABLED")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let tls_cert_path = env::var("TLS_CERT_PATH").unwrap_or_default();
    let tls_key_path  = env::var("TLS_KEY_PATH").unwrap_or_default();

    if tls_enabled {
        if tls_cert_path.is_empty() || tls_key_path.is_empty() {
            panic!("[ERROR] TLS_ENABLED=true の場合、TLS_CERT_PATH と TLS_KEY_PATH が必要だよ！");
        }
        println!("[設定] HTTPS有効: cert={} key={}", tls_cert_path, tls_key_path);
    }

    // ---------------------------------------------------------------
    // Webサーバー起動（Discordクライアントと並行して動かす）
    // ---------------------------------------------------------------
    let web_bind = env::var("WEB_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let web_bind_addr: std::net::SocketAddr = web_bind
        .parse()
        .expect("WEB_BINDのパースに失敗");

    let web_state = std::sync::Arc::new(web::WebState {
        db: db.clone(),
        http_client: http_client.clone(),
        memory_log_path: memory_log_path.clone(),
        image_save_dir: image_save_dir.clone(),
        vision_enabled,
        auth_user,
        auth_pass,
    });

    let app = web::create_router(web_state);

    let protocol = if tls_enabled { "https" } else { "http" };
    println!("[WEB] Webサーバーを起動するよ〜: {}://{}", protocol, web_bind_addr);

    tokio::spawn(async move {
        if tls_enabled {
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                &tls_cert_path,
                &tls_key_path,
            )
            .await
            .expect("TLS設定の読み込みに失敗したよ！証明書と秘密鍵のパスを確認してね。");

            axum_server::bind_rustls(web_bind_addr, config)
                .serve(app.into_make_service())
                .await
                .expect("HTTPSサーバーが落ちちゃった");
        } else {
            axum_server::bind(web_bind_addr)
                .serve(app.into_make_service())
                .await
                .expect("HTTPサーバーが落ちちゃった");
        }
    });

    // ---------------------------------------------------------------
    // Discord クライアント起動
    // ---------------------------------------------------------------
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&discord_token, intents)
        .event_handler(Handler {
            db,
            http_client,
            vision_enabled,
            memory_log_path,
            image_save_dir,
            admin_user_ids,
        })
        .await
        .expect("クライアントの作成に失敗しちゃった");

    if let Err(why) = client.start().await {
        println!("エラーが発生したよ: {:?}", why);
    }
}