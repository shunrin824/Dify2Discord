// ---------------------------------------------------------------
// src/web.rs
// 月見ヤチヨ Web チャットサーバー
// 2ch風シンプルBBS、古いブラウザ対応
// ---------------------------------------------------------------
use axum::{
    extract::{Form, State},
    response::{Html, Redirect},
    routing::{get, post},
    Router,
};
use chrono::Local;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::{env, sync::Arc};

use crate::{add_segment_to_dify, append_chat_log, consume_dify_stream, strip_think_tags};

// ---------------------------------------------------------------
// 共有状態
// ---------------------------------------------------------------
pub struct WebState {
    pub db: SqlitePool,
    pub http_client: HttpClient,
    pub memory_log_path: String,
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
        .with_state(state)
}

// ---------------------------------------------------------------
// / : フレームセットページ（フレーム非対応ブラウザ向けフォールバック付き）
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
        for (i, (ts, user_msg, bot_reply)) in entries.iter().rev().enumerate() {
            let num = i + 1;
            let user_esc = html_escape(user_msg);
            // <think>タグを除去してからHTMLエスケープ
            let bot_esc = html_escape(&strip_think_tags(bot_reply));
            // 改行を<br>に変換
            let bot_esc = bot_esc.replace('\n', "<br>");

            posts_html.push_str(&format!(
                r#"<dl class="post">
  <dt><span class="num">{num}</span> 名無しさん <span class="ts">{ts}</span></dt>
  <dd class="user-msg">{user_esc}</dd>
  <dt class="bot-header">↓ 月見ヤチヨ</dt>
  <dd class="bot-msg">{bot_esc}</dd>
</dl>
"#
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
// /chat : メッセージ入力フォーム
// ---------------------------------------------------------------
async fn chat_handler() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01 Transitional//EN"
  "http://www.w3.org/TR/html4/loose.dtd">
<html>
<head>
<meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
<title>入力</title>
<style type="text/css">
body {{
  font-family: MS PGothic, Osaka, sans-serif;
  font-size: 13px;
  background-color: #ddeeff;
  margin: 6px;
  padding: 0;
}}
p.label {{
  margin: 4px 0 4px 0;
  font-weight: bold;
  font-size: 13px;
  color: #333;
}}
form {{
  display: table;
  width: 100%;
}}
.form-row {{
  display: table-row;
}}
.form-row td {{
  display: table-cell;
  vertical-align: middle;
  padding: 3px;
}}
input[type="text"] {{
  width: 100%;
  padding: 5px;
  font-size: 13px;
  border: 1px solid #888;
  font-family: MS PGothic, Osaka, sans-serif;
  box-sizing: border-box;
}}
input[type="submit"] {{
  padding: 5px 14px;
  font-size: 13px;
  background-color: #ccccff;
  border: 1px solid #6666cc;
  cursor: pointer;
  white-space: nowrap;
  font-family: MS PGothic, Osaka, sans-serif;
}}
p.note {{
  font-size: 11px;
  color: #666;
  margin: 3px 0 0 0;
}}
</style>
</head>
<body>
<p class="label">ヤチヨに話しかける：</p>
<form method="POST" action="/send">
<table width="100%" cellspacing="0" cellpadding="3">
<tr>
  <td width="100%">
    <input type="text" name="message" maxlength="500" placeholder="メッセージを入力してください...">
  </td>
  <td>
    <input type="submit" value="送信">
  </td>
</tr>
</table>
</form>
<p class="note">送信後、上のログ欄に返事が表示されます（最大30秒ほどかかる場合があります）</p>
</body>
</html>"#,
    )
}

// ---------------------------------------------------------------
// /send : メッセージ送信（バックグラウンドでDify呼び出し、即座にリダイレクト）
// ---------------------------------------------------------------
#[derive(Deserialize)]
pub struct ChatForm {
    message: String,
}

async fn send_handler(
    State(state): State<Arc<WebState>>,
    Form(form): Form<ChatForm>,
) -> Redirect {
    let message = form.message.trim().to_string();

    if message.is_empty() {
        return Redirect::to("/chat");
    }

    // 会話IDをDBから取得
    let web_key = "web_web".to_string();
    let conv_id: String = sqlx::query_scalar(
        "SELECT conversation_id FROM conversations WHERE key = ?",
    )
    .bind(&web_key)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None)
    .unwrap_or_default();

    // バックグラウンドでDifyへ送信・保存
    let db = state.db.clone();
    let http_client = state.http_client.clone();
    let memory_log_path = state.memory_log_path.clone();

    tokio::spawn(async move {
        let dify_api_key = env::var("DIFY_API_KEY").unwrap_or_default();
        let dify_base_url =
            env::var("DIFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1".to_string());
        let dify_chat_url = format!("{}/v1/chat-messages", dify_base_url);
        let current_time = Local::now().format("%Y年%m月%d日 %H:%M").to_string();

        let payload = json!({
            "inputs": {"current_time": current_time},
            "query": message,
            "response_mode": "streaming",
            "conversation_id": conv_id,
            "user": "web_user"
        });

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

                        // ログとナレッジベースに保存
                        if let Err(e) =
                            append_chat_log(&memory_log_path, &message, &result.answer).await
                        {
                            eprintln!("[WEB WARN] ログ保存失敗: {}", e);
                        }
                        if let Err(e) =
                            add_segment_to_dify(&http_client, &message, &result.answer).await
                        {
                            eprintln!("[WEB WARN] Difyセグメント追加失敗: {}", e);
                        }
                    }
                }
            }
        }
    });

    // 即座にチャット入力フォームへ戻す
    Redirect::to("/chat")
}

// ---------------------------------------------------------------
// memory.md を読み込んで直近n件のエントリを返す
//
// memory.md の1エントリは以下の形式:
//   ## [YYYY-MM-DD HH:MM:SS] の会話
//   - shunrin824: ユーザーメッセージ
//   - 月見ヤチヨ: ボット返答（複数行・<think>タグ含む場合あり）
//
// ボット返答は複数行にわたる可能性があるため、次のセクション開始まで
// 続く行をすべて連結して取得する。
// ---------------------------------------------------------------
async fn read_last_entries(path: &str, n: usize) -> Vec<(String, String, String)> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[WEB WARN] memory.md 読み込み失敗: {}", e);
            return Vec::new();
        }
    };

    let mut entries: Vec<(String, String, String)> = Vec::new();
    let mut current_ts: Option<String> = None;
    let mut current_user: Option<String> = None;
    let mut current_bot: Option<String> = None;
    // ボット返答の続き行を収集中かどうかのフラグ
    let mut collecting_bot = false;

    for line in content.lines() {
        if line.starts_with("## [") && line.ends_with("] の会話") {
            // 直前のエントリを確定して追加
            if let (Some(ts), Some(u), Some(b)) =
                (current_ts.take(), current_user.take(), current_bot.take())
            {
                entries.push((ts, u, b));
            }
            collecting_bot = false;
            // タイムスタンプを抽出: "## [2024-01-01 12:00:00] の会話"
            let ts = line
                .trim_start_matches("## [")
                .trim_end_matches("] の会話")
                .to_string();
            current_ts = Some(ts);
            current_user = None;
            current_bot = None;
        } else if let Some(msg) = line.strip_prefix("- shunrin824: ") {
            collecting_bot = false;
            current_user = Some(msg.to_string());
        } else if let Some(msg) = line.strip_prefix("- 月見ヤチヨ: ") {
            // ボット返答の1行目
            current_bot = Some(msg.to_string());
            collecting_bot = true;
        } else if collecting_bot {
            // ボット返答の続き行（改行を挟んで連結）
            if let Some(ref mut bot) = current_bot {
                bot.push('\n');
                bot.push_str(line);
            }
        }
    }
    // 最後のエントリを追加
    if let (Some(ts), Some(u), Some(b)) = (current_ts, current_user, current_bot) {
        entries.push((ts, u, b));
    }

    // 末尾n件を返す
    let total = entries.len();
    if total > n {
        entries[total - n..].to_vec()
    } else {
        entries
    }
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
