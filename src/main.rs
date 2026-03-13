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

// NAS上のチャットログファイルパス
const MEMORY_LOG_PATH: &str = "/home/shunrin824/data16_1/yachiyo/chat_logs/memory.md";

// NAS上の画像保存ディレクトリ
const IMAGE_SAVE_DIR: &str = "/home/shunrin824/data16_1/yachiyo/images/";

// ---------------------------------------------------------------
// チャットログをNASのMarkdownファイルに非同期追記する
// ---------------------------------------------------------------
async fn append_chat_log(user_input: &str, bot_reply: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let entry = format!(
        "\n## [{}] の会話\n- shunrin824: {}\n- 月見ヤチヨ: {}\n",
        timestamp, user_input, bot_reply
    );
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(MEMORY_LOG_PATH)
        .await?;
    file.write_all(entry.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------
// Difyナレッジベースへリアルタイムでセグメントを追加する
// ---------------------------------------------------------------
async fn add_segment_to_dify(
    http_client: &HttpClient,
    user_input: &str,
    bot_reply: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dify_base_url = env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
    let dataset_api_key = env::var("DIFY_DATASET_API_KEY").expect("DIFY_DATASET_API_KEYが設定されていないよ！");
    let dataset_id = env::var("DIFY_DATASET_ID").expect("DIFY_DATASET_IDが設定されていないよ！");
    let document_id = env::var("DIFY_DOCUMENT_ID").expect("DIFY_DOCUMENT_IDが設定されていないよ！");

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
        "segments": [{
            "content": content,
            "keywords": []
        }]
    });

    let res = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", dataset_api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        return Err(format!("Difyセグメント追加エラー: {} / {}", status, body).into());
    }

    Ok(())
}

// ---------------------------------------------------------------
// 起動時: ナレッジベースとドキュメントを自動作成して.envに書き出す
// ---------------------------------------------------------------
pub async fn setup_dify_knowledge(http_client: &HttpClient) {
    // すでに設定済みならスキップ
    if env::var("DIFY_DATASET_ID").is_ok() && env::var("DIFY_DOCUMENT_ID").is_ok() {
        println!("ナレッジベース設定済み、セットアップをスキップするよ〜");
        return;
    }

    let dify_base_url = env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
    let dataset_api_key = env::var("DIFY_DATASET_API_KEY").expect("DIFY_DATASET_API_KEYが設定されていないよ！");

    // ナレッジベース（データセット）を新規作成
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
            let id = body["id"].as_str().expect("dataset_idが取れなかった").to_string();
            println!("ナレッジベース作成完了！ dataset_id: {}", id);
            id
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            panic!("ナレッジベース作成失敗: {} / レスポンス: {}", status, body);
        }
        Err(e) => panic!("ナレッジベース作成エラー: {:?}", e),
    };

    // 最初のドキュメントをテキストで作成（以降はセグメント追加で運用）
    let doc_res = http_client
        .post(format!("{}/v1/datasets/{}/document/create_by_text", dify_base_url, dataset_id))
        .header("Authorization", format!("Bearer {}", dataset_api_key))
        .header("Content-Type", "application/json")
        .json(&json!({
            "name": "chat_memory",
            "text": "# 月見ヤチヨの長期記憶\nこのドキュメントにはヤチヨとshunrin824の会話ログが蓄積されます。\n",
            "indexing_technique": "high_quality",
            "embedding_model": "bge-m3",
            "embedding_model_provider": "ollama",
            "process_rule": { "mode": "automatic" }
        }))
        .send()
        .await;

    let document_id = match doc_res {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.expect("レスポンスのパースに失敗");
            let id = body["document"]["id"].as_str().expect("document_idが取れなかった").to_string();
            println!("ドキュメント作成完了！ document_id: {}", id);
            id
        }
        Ok(r) => panic!("ドキュメント作成失敗: {}", r.status()),
        Err(e) => panic!("ドキュメント作成エラー: {:?}", e),
    };

    // .envファイルにIDを追記（次回起動時はスキップ）
    let env_addition = format!(
        "\nDIFY_DATASET_ID={}\nDIFY_DOCUMENT_ID={}\n",
        dataset_id, document_id
    );
    if let Ok(mut f) = OpenOptions::new().append(true).open(".env").await {
        let _ = f.write_all(env_addition.as_bytes()).await;
        println!(".envにIDを書き込んだよ〜！ 次回からはセットアップをスキップするよ");
    }

    // 現在のプロセスの環境変数にも反映
    unsafe {
        env::set_var("DIFY_DATASET_ID", &dataset_id);
        env::set_var("DIFY_DOCUMENT_ID", &document_id);
    }
}

// ---------------------------------------------------------------
// ルートA: 添付画像の処理
//   1. Discordからバイト列としてダウンロード
//   2. 元フォーマットのままNASに保存（思い出の宝箱）
//   3. imageクレートでJPEG品質100に変換
//   4. Dify /v1/files/upload にmultipartでアップロードし upload_file_id を返す
// ---------------------------------------------------------------

/// Discord添付ファイルのContent-TypeからMIMEを判定し、
/// NAS保存用の拡張子（元フォーマット）とDify送信用JPEG変換の要否を返す。
fn resolve_image_type(content_type: &str, filename: &str) -> Option<&'static str> {
    // Content-Typeが信頼できない場合はファイル名の拡張子でフォールバック
    let lower_ct = content_type.to_lowercase();
    let lower_fn = filename.to_lowercase();

    if lower_ct.contains("jpeg") || lower_ct.contains("jpg")
        || lower_fn.ends_with(".jpg") || lower_fn.ends_with(".jpeg") {
        Some("jpg")
    } else if lower_ct.contains("png") || lower_fn.ends_with(".png") {
        Some("png")
    } else if lower_ct.contains("webp") || lower_fn.ends_with(".webp") {
        Some("webp")
    } else {
        None // 対応外フォーマット
    }
}

/// 画像バイト列を受け取り、JPEG品質100のバイト列に変換して返す。
/// CPU負荷を避けるためspawn_blockingで同期処理をオフロードする。
async fn convert_to_jpeg_q100(raw_bytes: Vec<u8>) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let img = image::load_from_memory(&raw_bytes)?;
        let mut jpeg_buf: Vec<u8> = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, 100);
        encoder.encode_image(&img)?;
        Ok(jpeg_buf)
    })
    .await?
}

/// 1枚分の画像全処理:
///   NASに元フォーマット保存 → JPEG変換 → Difyアップロード → upload_file_id を返す
async fn process_single_attachment(
    http_client: &HttpClient,
    attachment_url: &str,
    original_filename: &str,
    content_type: &str,
    dify_api_key: &str,
    dify_base_url: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // ---- 1. DiscordのCDNからダウンロード ----
    let raw_bytes = http_client
        .get(attachment_url)
        .send()
        .await?
        .bytes()
        .await?
        .to_vec();

    // ---- 2. NASへ元フォーマットのまま保存 ----
    let ext = resolve_image_type(content_type, original_filename)
        .ok_or_else(|| format!("非対応の画像フォーマット: {} / {}", content_type, original_filename))?;

    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
    // 元ファイル名から拡張子を除いたベース名を安全化
    let stem = std::path::Path::new(original_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    // NASパスを構築
    let save_filename = format!("{}_{}.{}", timestamp, stem, ext);
    let save_path = format!("{}{}", IMAGE_SAVE_DIR, save_filename);

    // 保存ディレクトリを必要に応じて作成
    tokio::fs::create_dir_all(IMAGE_SAVE_DIR).await?;
    tokio::fs::write(&save_path, &raw_bytes).await?;
    println!("[画像保存] {}", save_path);

    // ---- 3. JPEG品質100に変換 ----
    let jpeg_bytes = convert_to_jpeg_q100(raw_bytes).await?;
    let upload_filename = format!("{}_{}.jpg", timestamp, stem);

    // ---- 4. Dify /v1/files/upload へmultipartアップロード ----
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
// Discord Event Handler
// ---------------------------------------------------------------

struct Handler {
    db: SqlitePool,
    http_client: HttpClient,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let dify_api_key = env::var("DIFY_API_KEY").expect("DIFY_API_KEYが設定されていないよ！");
        let dify_base_url = env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
        let dify_chat_url = format!("{}/v1/chat-messages", dify_base_url);
        let user_id = msg.author.id.to_string();
        let channel_id = msg.channel_id.to_string();
        let key = format!("{}_{}", user_id, channel_id);

        let _ = msg.channel_id.broadcast_typing(&ctx.http).await;

        // ---- 会話IDの取得 ----
        let conv_id: String = sqlx::query_scalar(
            "SELECT conversation_id FROM conversations WHERE key = ?"
        )
        .bind(&key)
        .fetch_optional(&self.db)
        .await
        .unwrap_or(None)
        .unwrap_or_default();

        // ================================================================
        // ルートA: 添付画像の処理
        // 画像添付がある場合、全画像をDifyにアップロードして files配列を構築
        // ================================================================
        let mut dify_files: Vec<serde_json::Value> = Vec::new();

        for attachment in &msg.attachments {
            let ct = attachment
                .content_type
                .clone()
                .unwrap_or_default();

            // 対応フォーマットかチェック（非対応はスキップ）
            if resolve_image_type(&ct, &attachment.filename).is_none() {
                println!("[スキップ] 非対応の添付ファイル: {} ({})", attachment.filename, ct);
                continue;
            }

            match process_single_attachment(
                &self.http_client,
                &attachment.url,
                &attachment.filename,
                &ct,
                &dify_api_key,
                &dify_base_url,
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
                    // 1枚失敗してもほかの処理は続行
                }
            }
        }

        // ================================================================
        // Dify chat-messages へ送信（テキスト + 画像ファイル）
        // ================================================================
        let payload = if dify_files.is_empty() {
            // テキストのみ（従来通り）
            json!({
                "inputs": {},
                "query": msg.content,
                "response_mode": "streaming",
                "conversation_id": conv_id,
                "user": &user_id
            })
        } else {
            // 画像あり
            json!({
                "inputs": {},
                "query": msg.content,
                "response_mode": "streaming",
                "conversation_id": conv_id,
                "user": &user_id,
                "files": dify_files
            })
        };

        let res = self.http_client
            .post(&dify_chat_url)
            .header("Authorization", format!("Bearer {}", dify_api_key))
            .json(&payload)
            .send()
            .await;

        match res {
            Ok(response) => {
                let mut stream = response.bytes_stream();
                let mut full_answer = String::new();
                let mut new_conv_id = String::new();

                while let Some(chunk) = stream.next().await {
                    if let Ok(bytes) = chunk {
                        let text = String::from_utf8_lossy(&bytes);
                        for line in text.lines() {
                            if let Some(json_str) = line.strip_prefix("data: ") {
                                if json_str == "[DONE]" { break; }
                                if let Ok(json_body) = serde_json::from_str::<serde_json::Value>(json_str) {
                                    if let Some(answer) = json_body["answer"].as_str() {
                                        full_answer.push_str(answer);
                                    }
                                    if let Some(cid) = json_body["conversation_id"].as_str() {
                                        new_conv_id = cid.to_string();
                                    }
                                }
                            }
                        }
                    }
                }

                if !new_conv_id.is_empty() {
                    sqlx::query(
                        "INSERT INTO conversations (key, conversation_id)
                         VALUES (?, ?)
                         ON CONFLICT(key) DO UPDATE SET conversation_id = excluded.conversation_id"
                    )
                    .bind(&key)
                    .bind(&new_conv_id)
                    .execute(&self.db)
                    .await
                    .ok();
                }

                if full_answer.is_empty() {
                    let _ = msg.channel_id.say(&ctx.http, "ごめんね、うまく返事が受け取れなかったみたい〜。").await;
                } else {
                    // Discordへ返答を送信
                    let _ = msg.channel_id.say(&ctx.http, &full_answer).await;

                    let user_input = msg.content.clone();
                    let bot_reply = full_answer.clone();
                    let http_client = self.http_client.clone();

                    // NASログ保存 & Difyセグメント追加を並行してバックグラウンド実行
                    tokio::spawn(async move {
                        if let Err(e) = append_chat_log(&user_input, &bot_reply).await {
                            eprintln!("[WARN] チャットログの保存に失敗したよ: {}", e);
                        }
                        if let Err(e) = add_segment_to_dify(&http_client, &user_input, &bot_reply).await {
                            eprintln!("[WARN] Difyへのセグメント追加に失敗したよ: {}", e);
                        }
                    });
                }
            }
            Err(e) => {
                println!("Difyへの接続エラー: {:?}", e);
                let _ = msg.channel_id.say(&ctx.http, "ごめんね、今ちょっと頭が回らなくてお返事できないみたい〜。").await;
            }
        }
    }

    async fn ready(&self, _: Context, ready: Ready) {
        println!("ヤチヨ、Discordにログイン完了したよ〜！ ({})", ready.user.name);
    }
}

// ---------------------------------------------------------------
// エントリポイント
// ---------------------------------------------------------------

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    let discord_token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKENが設定されていないよ！");

    let db = SqlitePool::connect("sqlite:conversations.db?mode=rwc")
        .await
        .expect("DBの接続に失敗しちゃった");

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS conversations (
            key TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL
        )"
    )
    .execute(&db)
    .await
    .expect("テーブルの作成に失敗しちゃった");

    let http_client = HttpClient::new();

    // 起動時にDifyナレッジベース＆ドキュメントを自動セットアップ
    setup_dify_knowledge(&http_client).await;

    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&discord_token, intents)
        .event_handler(Handler { db, http_client })
        .await
        .expect("クライアントの作成に失敗しちゃった");

    if let Err(why) = client.start().await {
        println!("エラーが発生したよ: {:?}", why);
    }
}
