// ---------------------------------------------------------------
// src/web.rs
// 月見ヤチヨ Web チャットサーバー
// 2ch風シンプルBBS、古いブラウザ対応
//
// 追加機能:
//   - 入力モード切り替え（1行 Enter送信 / テキストエリア 改行可）
//   - 複数画像投稿、WebP 長辺1024リサイズして保存・表示
//   - Basic認証（WEB_AUTH_USER / WEB_AUTH_PASS）
//   - HTTPS対応（TLS_ENABLED / TLS_CERT_PATH / TLS_KEY_PATH）※main.rs 側で制御
// ---------------------------------------------------------------
use axum::{
    body::Body,
    extract::{Multipart, Path, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose, Engine as _};
use chrono::Local;
use reqwest::Client as HttpClient;
use serde_json::json;
use sqlx::SqlitePool;
use std::{env, sync::Arc};

use crate::{
    add_segment_to_dify, append_chat_log, consume_dify_stream, resolve_image_type, strip_think_tags,
};

// ---------------------------------------------------------------
// 共有状態
// ---------------------------------------------------------------
pub struct WebState {
    pub db: SqlitePool,
    pub http_client: HttpClient,
    pub memory_log_path: String,
    /// 画像保存ディレクトリ（web用は {image_save_dir}/web/ に保存）
    pub image_save_dir: String,
    /// vision機能が有効な場合にDifyへ画像をアップロードする
    pub vision_enabled: bool,
    /// Basic認証ユーザー名（Noneなら認証不要）
    pub auth_user: Option<String>,
    /// Basic認証パスワード
    pub auth_pass: Option<String>,
}

// ---------------------------------------------------------------
// Basic認証ミドルウェア
// WEB_AUTH_USER が未設定の場合はスルーする
// ---------------------------------------------------------------
pub async fn basic_auth_middleware(
    State(state): State<Arc<WebState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // 認証設定がなければそのまま通す
    let Some(ref expected_user) = state.auth_user else {
        return next.run(request).await;
    };
    let expected_pass = state.auth_pass.as_deref().unwrap_or("");

    let authorized = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Basic "))
        .and_then(|b64| general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|creds| creds == format!("{}:{}", expected_user, expected_pass))
        .unwrap_or(false);

    if authorized {
        next.run(request).await
    } else {
        Response::builder()
            .status(401)
            .header("WWW-Authenticate", r#"Basic realm="Yachiyo Chat""#)
            .body(Body::from("Unauthorized"))
            .unwrap()
    }
}

// ---------------------------------------------------------------
// ルーター定義
// ---------------------------------------------------------------
pub fn create_router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/messages", get(messages_handler))
        .route("/chat", get(chat_handler))
        .route("/send", post(send_handler))
        // web経由で保存した画像を配信するエンドポイント
        .route("/images/web/:filename", get(serve_image_handler))
        // 全ルートにBasic認証ミドルウェアを適用
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            basic_auth_middleware,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------
// 画像を WebP 形式、長辺最大1024px にリサイズして返す
// ---------------------------------------------------------------
async fn resize_to_webp_1024(
    raw_bytes: Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&raw_bytes)?;
        let img = if img.width() > 1024 || img.height() > 1024 {
            img.resize(1024, 1024, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let mut cursor = std::io::Cursor::new(Vec::new());
        img.write_to(&mut cursor, image::ImageFormat::WebP)?;
        Ok::<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>(cursor.into_inner())
    })
    .await?
}

// ---------------------------------------------------------------
// WebP 画像バイト列を Dify にアップロードして upload_file_id を返す
// ---------------------------------------------------------------
async fn upload_webp_to_dify(
    http_client: &HttpClient,
    bytes: Vec<u8>,
    filename: String,
    dify_api_key: &str,
    dify_base_url: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename)
        .mime_str("image/webp")?;
    let form = reqwest::multipart::Form::new()
        .text("type", "image")
        .part("file", part);

    let res = http_client
        .post(format!("{}/v1/files/upload", dify_base_url))
        .header("Authorization", format!("Bearer {}", dify_api_key))
        .multipart(form)
        .send()
        .await?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        return Err(format!("Dify画像アップロード失敗: {} / {}", status, body).into());
    }

    let body: serde_json::Value = res.json().await?;
    let id = body["id"]
        .as_str()
        .ok_or("upload_file_idが取得できなかった")?
        .to_string();
    Ok(id)
}

// ---------------------------------------------------------------
// /images/web/:filename : 保存済みWebP画像を配信する
// ---------------------------------------------------------------
async fn serve_image_handler(
    State(state): State<Arc<WebState>>,
    Path(filename): Path<String>,
) -> impl IntoResponse {
    // パストラバーサル防止
    if filename.contains('/') || filename.contains("..") || filename.contains('\\') {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let path = format!(
        "{}/web/{}",
        state.image_save_dir.trim_end_matches('/'),
        filename
    );

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ct = if filename.ends_with(".webp") {
                "image/webp"
            } else if filename.ends_with(".jpg") || filename.ends_with(".jpeg") {
                "image/jpeg"
            } else if filename.ends_with(".png") {
                "image/png"
            } else {
                "application/octet-stream"
            };
            ([(axum::http::header::CONTENT_TYPE, ct)], bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------
// / : フレームセットページ
// ---------------------------------------------------------------
async fn index_handler() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01 Frameset//EN"
  "http://www.w3.org/TR/html4/frameset.dtd">
<html>
<head>
<meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
<title>月見ヤチヨ チャット</title>
</head>
<frameset rows="70%,30%" border="1" framespacing="2">
  <frame src="/messages" name="messages" scrolling="yes" marginwidth="6" marginheight="6">
  <frame src="/chat"     name="chat"     scrolling="no"  marginwidth="6" marginheight="6" noresize>
  <noframes>
  <body>
  <p>このページを表示するにはフレームに対応したブラウザが必要です。</p>
  </body>
  </noframes>
</frameset>
</html>"#,
    )
}

// ---------------------------------------------------------------
// /messages : 直近5件の会話を表示（10秒ごとに自動更新）
// ---------------------------------------------------------------
async fn messages_handler(State(state): State<Arc<WebState>>) -> Html<String> {
    let entries = read_last_entries(&state.memory_log_path, 5).await;

    let mut posts_html = String::new();

    if entries.is_empty() {
        posts_html.push_str(
            r#"<p class="empty">まだ会話ログがないよ〜。下のフォームから話しかけてみてね！</p>"#,
        );
    } else {
        for (i, entry) in entries.iter().rev().enumerate() {
            let num = i + 1;
            let user_esc = html_escape(&entry.user_text);
            let bot_esc = html_escape(&strip_think_tags(&entry.bot_reply))
                .replace('\n', "<br>");

            // 投稿済み画像のサムネイル HTML を組み立てる
            let images_html = if entry.image_filenames.is_empty() {
                String::new()
            } else {
                let mut imgs = String::from(r#"<dd class="img-row">"#);
                for fname in &entry.image_filenames {
                    let fname_esc = html_escape(fname);
                    imgs.push_str(&format!(
                        r#"<a href="/images/web/{fname_esc}" target="_blank"><img src="/images/web/{fname_esc}" class="thumb" alt="添付画像"></a>"#,
                    ));
                }
                imgs.push_str("</dd>");
                imgs
            };

            posts_html.push_str(&format!(
                r#"<dl class="post">
  <dt><span class="num">{num}</span> 名無しさん <span class="ts">{ts}</span></dt>
  {images_html}<dd class="user-msg">{user_esc}</dd>
  <dt class="bot-header">↓ 月見ヤチヨ</dt>
  <dd class="bot-msg">{bot_esc}</dd>
</dl>
"#,
                num = num,
                ts = html_escape(&entry.timestamp),
                images_html = images_html,
                user_esc = user_esc,
                bot_esc = bot_esc,
            ));
        }
    }

    Html(format!(
        r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01 Transitional//EN"
  "http://www.w3.org/TR/html4/loose.dtd">
<html>
<head>
<meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
<meta http-equiv="refresh" content="10">
<title>ログ</title>
<style type="text/css">
body {{
  font-family: MS PGothic, Osaka, sans-serif;
  font-size: 13px;
  background-color: #efefef;
  color: #000;
  margin: 6px;
  padding: 0;
}}
h2 {{
  font-size: 14px;
  border-bottom: 2px solid #800000;
  padding-bottom: 3px;
  margin-bottom: 8px;
  color: #800000;
}}
dl.post {{
  background-color: #fff;
  border: 1px solid #bbb;
  margin: 0 0 8px 0;
  padding: 5px 8px;
}}
dt {{
  font-size: 12px;
  color: #333;
  font-weight: bold;
  margin-bottom: 3px;
}}
.num {{
  color: #000080;
  margin-right: 6px;
}}
.ts {{
  color: #666;
  font-weight: normal;
  margin-left: 8px;
}}
dd {{
  margin: 2px 0 6px 12px;
  line-height: 1.6;
  word-break: break-all;
}}
.user-msg {{
  color: #222;
}}
.bot-header {{
  color: #800000;
  font-size: 12px;
  margin-top: 4px;
}}
.bot-msg {{
  color: #000066;
}}
.empty {{
  color: #666;
  font-style: italic;
}}
.img-row {{
  margin: 4px 0 4px 12px;
  display: block;
}}
.img-row a {{
  display: inline-block;
  margin-right: 4px;
  margin-bottom: 4px;
}}
.thumb {{
  max-width: 160px;
  max-height: 160px;
  border: 1px solid #aaa;
  vertical-align: top;
  cursor: pointer;
}}
p.footer {{
  font-size: 11px;
  color: #888;
  text-align: right;
  margin-top: 4px;
}}
</style>
</head>
<body>
<h2>月見ヤチヨ チャット ─ 最近の会話（直近5件）</h2>
{posts_html}
<p class="footer">（10秒ごとに自動更新）</p>
</body>
</html>"#
    ))
}

// ---------------------------------------------------------------
// /chat : メッセージ入力フォーム（入力モード切り替え＋画像添付対応）
// ---------------------------------------------------------------
async fn chat_handler() -> Html<String> {
    Html(r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01 Transitional//EN"
  "http://www.w3.org/TR/html4/loose.dtd">
<html>
<head>
<meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
<title>入力</title>
<style type="text/css">
body {
  font-family: MS PGothic, Osaka, sans-serif;
  font-size: 13px;
  background-color: #ddeeff;
  margin: 6px;
  padding: 0;
}
p.label {
  margin: 4px 0 4px 0;
  font-weight: bold;
  font-size: 13px;
  color: #333;
}
table.form-table {
  width: 100%;
  border-collapse: collapse;
}
table.form-table td {
  vertical-align: middle;
  padding: 3px;
}
td.input-cell {
  width: 100%;
}
#msg-input {
  width: 100%;
  padding: 5px;
  font-size: 13px;
  border: 1px solid #888;
  font-family: MS PGothic, Osaka, sans-serif;
  box-sizing: border-box;
}
#msg-textarea {
  width: 100%;
  padding: 5px;
  font-size: 13px;
  border: 1px solid #888;
  font-family: MS PGothic, Osaka, sans-serif;
  box-sizing: border-box;
  resize: vertical;
}
input[type="submit"], input[type="button"] {
  padding: 5px 10px;
  font-size: 12px;
  background-color: #ccccff;
  border: 1px solid #6666cc;
  cursor: pointer;
  white-space: nowrap;
  font-family: MS PGothic, Osaka, sans-serif;
}
.file-row {
  margin-top: 4px;
}
input[type="file"] {
  font-size: 12px;
  font-family: MS PGothic, Osaka, sans-serif;
}
p.note {
  font-size: 11px;
  color: #555;
  margin: 3px 0 0 0;
}
p.mode-note {
  font-size: 11px;
  color: #666;
  margin: 2px 0 0 0;
}
</style>
<script type="text/javascript">
// 現在のモード: false=1行（Enter送信）, true=テキストエリア（改行可）
var textareaMode = false;

function getCurrentValue() {
  var el = document.getElementById('msg-input') || document.getElementById('msg-textarea');
  return el ? el.value : '';
}

function bindEnterSend(el) {
  el.onkeydown = function(e) {
    var key = e.keyCode || e.which;
    if (key == 13) {
      submitForm();
      return false;
    }
  };
}

function submitForm() {
  var f = document.getElementById('chat-form');
  if (f) f.submit();
}

function toggleMode() {
  var curVal = getCurrentValue();
  var cell = document.getElementById('input-cell');
  var btn  = document.getElementById('toggle-btn');
  var note = document.getElementById('mode-note');
  textareaMode = !textareaMode;

  if (textareaMode) {
    // テキストエリアモード: Shift+Enter で送信、Enter は改行
    var ta = document.createElement('textarea');
    ta.id = 'msg-textarea';
    ta.name = 'message';
    ta.rows = 3;
    ta.maxLength = 2000;
    ta.placeholder = 'メッセージを入力... （Shift+Enter で送信）';
    ta.onkeydown = function(e) {
      var key = e.keyCode || e.which;
      if (key == 13 && e.shiftKey) {
        submitForm();
        return false;
      }
    };
    cell.innerHTML = '';
    cell.appendChild(ta);
    ta.value = curVal;
    ta.focus();
    btn.value = '1行モードに切替';
    note.innerHTML = 'テキストエリアモード（Enter=改行 / Shift+Enter=送信）';
  } else {
    // 1行モード: Enter で送信
    var inp = document.createElement('input');
    inp.type = 'text';
    inp.id = 'msg-input';
    inp.name = 'message';
    inp.maxLength = 500;
    inp.placeholder = 'メッセージを入力... （Enter で送信）';
    bindEnterSend(inp);
    cell.innerHTML = '';
    cell.appendChild(inp);
    inp.value = curVal;
    inp.focus();
    btn.value = 'テキストエリアモードに切替';
    note.innerHTML = '1行モード（Enter=送信）';
  }
}

window.onload = function() {
  // デフォルト: 1行モード / Enter送信
  var inp = document.getElementById('msg-input');
  if (inp) {
    bindEnterSend(inp);
    inp.focus();
  }
};
</script>
</head>
<body>
<p class="label">ヤチヨに話しかける：</p>
<form id="chat-form" method="POST" action="/send" enctype="multipart/form-data">
<table class="form-table">
<tr>
  <td class="input-cell" id="input-cell">
    <input type="text" id="msg-input" name="message" maxlength="500"
           placeholder="メッセージを入力... （Enter で送信）">
  </td>
  <td>
    <input type="submit" value="送信">
  </td>
</tr>
</table>
<div class="file-row">
  <input type="file" name="images" accept="image/jpeg,image/png,image/webp" multiple>
  &nbsp;
  <input type="button" id="toggle-btn" value="テキストエリアモードに切替"
         onclick="toggleMode()">
</div>
</form>
<p id="mode-note" class="mode-note">1行モード（Enter=送信）</p>
<p class="note">※ 画像はJPEG・PNG・WebP対応。送信後、上のログ欄に返事が表示されます（最大30秒かかる場合があります）</p>
</body>
</html>"#.to_string())
}

// ---------------------------------------------------------------
// /send : multipart/form-data を受け取る
//   - message フィールド: テキスト
//   - images  フィールド: 画像ファイル（複数可）
// ---------------------------------------------------------------
async fn send_handler(
    State(state): State<Arc<WebState>>,
    mut multipart: Multipart,
) -> Redirect {
    let mut message = String::new();
    // (WebPバイト列, 保存ファイル名)
    let mut processed_images: Vec<(Vec<u8>, String)> = Vec::new();

    // ── multipart フィールドを順番に処理 ──────────────────────
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();

        if name == "message" {
            message = field.text().await.unwrap_or_default();
            continue;
        }

        if name != "images" {
            continue;
        }

        let orig_filename = field.file_name().unwrap_or("image.jpg").to_string();
        let ct = field.content_type().unwrap_or("image/jpeg").to_string();

        // 対応フォーマットチェック
        if resolve_image_type(&ct, &orig_filename).is_none() {
            println!("[WEB] スキップ（非対応フォーマット）: {} / {}", orig_filename, ct);
            continue;
        }

        let raw_bytes = match field.bytes().await {
            Ok(b) if !b.is_empty() => b.to_vec(),
            _ => continue,
        };

        // タイムスタンプベースのユニークなファイル名
        let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
        let save_filename = format!("web_{}.webp", timestamp);

        // リサイズ → WebP変換
        match resize_to_webp_1024(raw_bytes).await {
            Ok(webp) => {
                processed_images.push((webp, save_filename));
            }
            Err(e) => {
                eprintln!("[WEB WARN] 画像リサイズ失敗 ({}): {}", orig_filename, e);
            }
        }
    }

    let message = message.trim().to_string();

    // テキストも画像もなければ何もしない
    if message.is_empty() && processed_images.is_empty() {
        return Redirect::to("/chat");
    }

    // ── 画像をディスクに保存 ──────────────────────────────────
    let web_image_dir = format!(
        "{}/web",
        state.image_save_dir.trim_end_matches('/')
    );

    let mut saved_filenames: Vec<String> = Vec::new();
    for (webp_bytes, filename) in &processed_images {
        if let Err(e) = tokio::fs::create_dir_all(&web_image_dir).await {
            eprintln!("[WEB WARN] 画像ディレクトリ作成失敗: {}", e);
            continue;
        }
        let save_path = format!("{}/{}", web_image_dir, filename);
        if let Err(e) = tokio::fs::write(&save_path, webp_bytes).await {
            eprintln!("[WEB WARN] 画像保存失敗 ({}): {}", filename, e);
        } else {
            println!("[WEB] 画像保存: {}", save_path);
            saved_filenames.push(filename.clone());
        }
    }

    // ── メモリログ用メッセージ（画像ファイル名を埋め込む）────
    // 形式: [画像: file1.webp|file2.webp] テキスト
    let log_message = if saved_filenames.is_empty() {
        message.clone()
    } else {
        format!("[画像: {}] {}", saved_filenames.join("|"), message)
    };

    // ── Dify へ送るクエリテキスト ─────────────────────────────
    let dify_query = if message.is_empty() && !saved_filenames.is_empty() {
        "（画像を送ったよ）".to_string()
    } else {
        message.clone()
    };

    // ── 会話IDをDBから取得 ────────────────────────────────────
    let web_key = "web_web".to_string();
    let conv_id: String =
        sqlx::query_scalar("SELECT conversation_id FROM conversations WHERE key = ?")
            .bind(&web_key)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None)
            .unwrap_or_default();

    // ── バックグラウンドで Dify 送信・ログ保存 ───────────────
    let db            = state.db.clone();
    let http_client   = state.http_client.clone();
    let memory_log_path = state.memory_log_path.clone();
    let vision_enabled  = state.vision_enabled;

    tokio::spawn(async move {
        let dify_api_key = env::var("DIFY_API_KEY").unwrap_or_default();
        let dify_base_url =
            env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
        let dify_chat_url = format!("{}/v1/chat-messages", dify_base_url);
        let current_time = Local::now().format("%Y年%m月%d日 %H:%M").to_string();

        // vision が有効な場合は画像を Dify にもアップロードする
        let mut dify_files: Vec<serde_json::Value> = Vec::new();
        if vision_enabled {
            for (webp_bytes, filename) in &processed_images {
                match upload_webp_to_dify(
                    &http_client,
                    webp_bytes.clone(),
                    filename.clone(),
                    &dify_api_key,
                    &dify_base_url,
                )
                .await
                {
                    Ok(file_id) => {
                        dify_files.push(json!({
                            "type": "image",
                            "transfer_method": "local_file",
                            "upload_file_id": file_id
                        }));
                    }
                    Err(e) => {
                        eprintln!("[WEB WARN] Dify画像アップロード失敗 ({}): {}", filename, e);
                    }
                }
            }
        }

        let mut payload = json!({
            "inputs": {"current_time": current_time},
            "query": dify_query,
            "response_mode": "streaming",
            "conversation_id": conv_id,
            "user": "web_user"
        });
        if !dify_files.is_empty() {
            payload["files"] = json!(dify_files);
        }

        let res = http_client
            .post(&dify_chat_url)
            .header("Authorization", format!("Bearer {}", dify_api_key))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await;

        match res {
            Err(e) => eprintln!("[WEB ERROR] Dify接続エラー: {:?}", e),
            Ok(response) if !response.status().is_success() => {
                eprintln!("[WEB ERROR] Dify HTTP {}", response.status());
            }
            Ok(response) => {
                match consume_dify_stream(response).await {
                    Err(e) => eprintln!("[WEB ERROR] ストリームパースエラー: {:?}", e),
                    Ok(result) => {
                        if !result.conversation_id.is_empty() {
                            sqlx::query(
                                "INSERT INTO conversations (key, conversation_id)
                                 VALUES (?, ?)
                                 ON CONFLICT(key) DO UPDATE SET conversation_id = excluded.conversation_id",
                            )
                            .bind(&web_key)
                            .bind(&result.conversation_id)
                            .execute(&db)
                            .await
                            .ok();
                        }

                        if result.answer.is_empty() {
                            eprintln!("[WEB WARN] Difyからの回答が空");
                            return;
                        }

                        // ログ・ナレッジベースへ保存（画像ファイル名込みの log_message を使用）
                        if let Err(e) =
                            append_chat_log(&memory_log_path, &log_message, &result.answer).await
                        {
                            eprintln!("[WEB WARN] ログ保存失敗: {}", e);
                        }
                        if let Err(e) =
                            add_segment_to_dify(&http_client, &log_message, &result.answer).await
                        {
                            eprintln!("[WEB WARN] Difyセグメント追加失敗: {}", e);
                        }
                    }
                }
            }
        }
    });

    Redirect::to("/chat")
}

// ---------------------------------------------------------------
// ChatEntry: read_last_entries の戻り値
// ---------------------------------------------------------------
struct ChatEntry {
    timestamp: String,
    /// memory.md から抽出した画像ファイル名リスト
    image_filenames: Vec<String>,
    /// ユーザー発言テキスト（画像プレフィックス除去済み）
    user_text: String,
    /// ヤチヨの返答（<think>タグ込みの生テキスト）
    bot_reply: String,
}

// ---------------------------------------------------------------
// memory.md を読み込んで直近n件のエントリを返す
//
// ユーザー発言行の形式:
//   - shunrin824: [画像: file1.webp|file2.webp] テキスト  ← 画像あり
//   - shunrin824: テキスト                                 ← 画像なし
// ---------------------------------------------------------------
async fn read_last_entries(path: &str, n: usize) -> Vec<ChatEntry> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[WEB WARN] memory.md 読み込み失敗: {}", e);
            return Vec::new();
        }
    };

    let mut entries: Vec<ChatEntry> = Vec::new();

    let mut current_ts: Option<String>       = None;
    let mut current_user: Option<String>     = None;
    let mut current_images: Vec<String>      = Vec::new();
    let mut current_bot: Option<String>      = None;
    let mut collecting_bot                   = false;

    for line in content.lines() {
        if line.starts_with("## [") && line.ends_with("] の会話") {
            // 前のエントリを確定
            if let (Some(ts), Some(u), Some(b)) =
                (current_ts.take(), current_user.take(), current_bot.take())
            {
                entries.push(ChatEntry {
                    timestamp: ts,
                    image_filenames: std::mem::take(&mut current_images),
                    user_text: u,
                    bot_reply: b,
                });
            }
            current_images.clear();
            collecting_bot = false;

            let ts = line
                .trim_start_matches("## [")
                .trim_end_matches("] の会話")
                .to_string();
            current_ts   = Some(ts);
            current_user = None;
            current_bot  = None;
        } else if let Some(raw) = line.strip_prefix("- shunrin824: ") {
            collecting_bot = false;
            let (imgs, text) = parse_user_message(raw);
            current_images = imgs;
            current_user   = Some(text);
        } else if let Some(msg) = line.strip_prefix("- 月見ヤチヨ: ") {
            current_bot    = Some(msg.to_string());
            collecting_bot = true;
        } else if collecting_bot {
            if let Some(ref mut bot) = current_bot {
                bot.push('\n');
                bot.push_str(line);
            }
        }
    }

    // 最後のエントリを追加
    if let (Some(ts), Some(u), Some(b)) = (current_ts, current_user, current_bot) {
        entries.push(ChatEntry {
            timestamp: ts,
            image_filenames: current_images,
            user_text: u,
            bot_reply: b,
        });
    }

    let total = entries.len();
    if total > n {
        entries[total - n..].to_vec()
    } else {
        entries
    }
}

// Vec<ChatEntry> が collect できるよう Clone を実装
impl Clone for ChatEntry {
    fn clone(&self) -> Self {
        Self {
            timestamp:       self.timestamp.clone(),
            image_filenames: self.image_filenames.clone(),
            user_text:       self.user_text.clone(),
            bot_reply:       self.bot_reply.clone(),
        }
    }
}

// ---------------------------------------------------------------
// ユーザーメッセージ文字列を (画像ファイル名リスト, テキスト) に分解する
//
// "[画像: file1.webp|file2.webp] テキスト" → (["file1.webp","file2.webp"], "テキスト")
// "テキストのみ" → ([], "テキストのみ")
// ---------------------------------------------------------------
fn parse_user_message(raw: &str) -> (Vec<String>, String) {
    if let Some(rest) = raw.strip_prefix("[画像: ") {
        if let Some(bracket_end) = rest.find(']') {
            let files: Vec<String> = rest[..bracket_end]
                .split('|')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // "] " の後ろがテキスト（存在しない場合は空文字）
            let text = rest[bracket_end + 1..].trim_start_matches(' ').to_string();
            return (files, text);
        }
    }
    (Vec::new(), raw.to_string())
}

// ---------------------------------------------------------------
// HTMLエスケープ
// ---------------------------------------------------------------
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}