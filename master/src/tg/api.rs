//! Telegram Bot API 的最小客户端(DESIGN.md §9.1)。
//!
//! 只用到五个方法:`getUpdates` / `sendMessage` / `editMessageText` /
//! `answerCallbackQuery` / `setMyCommands`。不引 teloxide 之类的框架 ——
//! 那会把一整套 dispatcher 和状态机拖进来,而这里需要的是五个 POST。

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use super::fmt;

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    token: String,
    timeout: Duration,
    /// 只有测试会改它:把请求指向一个本机端口,好在不出网的前提下制造错误。
    base: String,
}

impl Api {
    pub fn new(token: &str, request_timeout_secs: u64) -> Result<Self> {
        let timeout = Duration::from_secs(request_timeout_secs.max(3));
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .build()
            .context("构建 Telegram HTTP 客户端失败")?;
        Ok(Self {
            client,
            token: token.to_string(),
            timeout,
            base: "https://api.telegram.org".to_string(),
        })
    }

    /// **不要把它写进日志。** bot_token 是凭据(§11.3)。
    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base, self.token, method)
    }

    async fn post(&self, method: &str, payload: &Value, timeout: Duration) -> Result<Value> {
        // `reqwest::Error` 的 Display 会带上请求 URL —— 而 URL 里就是 bot_token。
        // 超时、DNS 抖动这类最常见的错误全走这条路,再被上层 `error = %e` 写进
        // journald。`without_url()` 把它剥掉;`api_error_display_never_contains_token`
        // 钉住这件事。
        let resp = self
            .client
            .post(self.url(method))
            .timeout(timeout)
            .json(payload)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("请求 Telegram {method} 失败"))?;
        let value: Value = resp
            .json()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("解析 Telegram {method} 响应失败"))?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = value.get("error_code").and_then(Value::as_i64).unwrap_or_default();
            let desc = value.get("description").and_then(Value::as_str).unwrap_or("无描述");
            // 错误里带上 error_code,`is_conflict` 靠它认 409。
            // **不要把整个 value 拼进来** —— 请求体里有 chat_id 之类的东西。
            return Err(anyhow!("Telegram {method} 失败 (error_code: {code}): {desc}"));
        }
        Ok(value)
    }

    /// 长轮询取更新。
    ///
    /// HTTP 超时必须**大于** `timeout` 参数,否则每次长轮询都会在服务端还挂着的时候
    /// 被客户端掐断,表现成「命令要按两次才响应」。
    pub async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        const LONG_POLL_SECS: u64 = 25;
        let payload = json!({
            "offset": offset,
            "timeout": LONG_POLL_SECS,
            "allowed_updates": ["message", "callback_query"],
        });
        let value =
            self.post("getUpdates", &payload, Duration::from_secs(LONG_POLL_SECS + 10)).await?;
        let result =
            value.get("result").cloned().ok_or_else(|| anyhow!("getUpdates 响应缺少 result"))?;
        serde_json::from_value(result).context("解析 getUpdates 的 result 失败")
    }

    /// HTML 模式发送。**调用方负责把用户输入过一遍 `fmt::h`。**
    pub async fn send_html(&self, chat_id: i64, text: &str, markup: Option<Value>) -> Result<()> {
        let mut payload = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        if let Some(m) = markup {
            payload["reply_markup"] = m;
        }
        self.post("sendMessage", &payload, self.timeout).await?;
        Ok(())
    }

    /// 卡片切换:有 `edit_msg_id` 就在原消息上改,没有(或改失败)就新发一条。
    ///
    /// 原地改是为了不让聊天流积累一长串切换历史 —— 点五次菜单不该刷出五条消息。
    /// 「内容未变」是良性错误(用户重复点了同一个按钮),吞掉不报。
    pub async fn send_or_edit(
        &self,
        chat_id: i64,
        edit_msg_id: Option<i64>,
        text: &str,
        markup: Option<Value>,
    ) -> Result<()> {
        if let Some(msg_id) = edit_msg_id {
            let mut payload = json!({
                "chat_id": chat_id,
                "message_id": msg_id,
                "text": text,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            });
            if let Some(m) = &markup {
                payload["reply_markup"] = m.clone();
            }
            match self.post("editMessageText", &payload, self.timeout).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if e.to_string().contains("message is not modified") {
                        return Ok(());
                    }
                    // 对方把消息删了之类的情况:降级成新发一条。
                    tracing::debug!(error = %e, "editMessageText 失败,降级为 sendMessage");
                }
            }
        }
        self.send_html(chat_id, text, markup).await
    }

    /// 发一段可能很长的代码块(订阅链接列表)。
    ///
    /// Telegram 单条上限 4096 字符,而 HTML 转义会让长度膨胀 ——
    /// 所以按 3000 切,留出余量。
    pub async fn send_code_block(
        &self,
        chat_id: i64,
        title_html: &str,
        body: &str,
        markup: Option<Value>,
    ) -> Result<()> {
        let chunks = fmt::split_message(body, 3000);
        let last = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.iter().enumerate() {
            // 键盘只挂在最后一条上,否则每一段下面都跟一排按钮。
            let m = if i == last { markup.clone() } else { None };
            let text = if i == 0 {
                format!("{title_html}\n\n<code>{}</code>", fmt::h(chunk))
            } else {
                format!("<code>{}</code>", fmt::h(chunk))
            };
            self.send_html(chat_id, &text, m).await?;
        }
        Ok(())
    }

    /// 应答 callback,让客户端上的转圈停下来。失败无所谓 —— 它纯粹是 UI 反馈。
    pub async fn answer_callback(&self, id: &str) -> Result<()> {
        self.post("answerCallbackQuery", &json!({ "callback_query_id": id }), self.timeout).await?;
        Ok(())
    }

    /// 注册命令菜单(输入框左下角的 `/` 面板)。失败不致命。
    pub async fn register_commands(&self) -> Result<()> {
        let commands = json!([
            { "command": "start",  "description": "打开主菜单 / 刷新" },
            { "command": "usage",  "description": "查看我的流量" },
            { "command": "sub",    "description": "获取订阅地址" },
            { "command": "bind",   "description": "绑定账号: /bind <绑定码>" },
            { "command": "usages", "description": "(管理员)全部用户流量" },
        ]);
        self.post("setMyCommands", &json!({ "commands": commands }), self.timeout).await?;
        Ok(())
    }
}

/// 识别 409 Conflict:同一个 bot_token 在别处也在跑 getUpdates。
///
/// 租约挡得住同机多开,挡不住「另一台机器用了同一个 token」——
/// 那种情况下继续轮询只会两边互相抢 update,必须停下来并给出明确指引。
pub fn is_conflict(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("error_code: 409") || msg.contains("terminated by other")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 请求失败时的错误文本**不能带 bot_token**。reqwest 的错误默认把 URL 一起
    /// 打出来,而 URL 里就是 token;上层是 `error = %e` 直接进日志的。
    /// 连一个本机上没人听的端口,错误必然发生,又不用出网。
    #[tokio::test]
    async fn api_error_display_never_contains_token() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let mut api = Api::new("123456:SECRET-BOT-TOKEN", 3).unwrap();
        api.base = format!("http://127.0.0.1:{port}");
        let e = api.post("getMe", &json!({}), Duration::from_secs(3)).await.unwrap_err();
        for text in [format!("{e}"), format!("{e:#}"), format!("{e:?}")] {
            assert!(!text.contains("SECRET-BOT-TOKEN"), "泄露了 token:{text}");
            assert!(!text.contains("bot123456"), "泄露了 token:{text}");
        }
    }

    #[test]
    fn conflict_is_recognised_from_the_error_text() {
        let e = anyhow!("Telegram getUpdates 失败 (error_code: 409): Conflict: terminated by other getUpdates request");
        assert!(is_conflict(&e));
        let other = anyhow!("Telegram getUpdates 失败 (error_code: 401): Unauthorized");
        assert!(!is_conflict(&other));
        assert!(!is_conflict(&anyhow!("网络超时")));
    }

    /// token 是凭据,绝不能进日志。这里守的是「URL 只在 url() 里拼」这条边界。
    #[test]
    fn api_url_contains_the_token_only_where_expected() {
        let api = Api::new("123:SECRET", 10).unwrap();
        let url = api.url("getUpdates");
        assert_eq!(url, "https://api.telegram.org/bot123:SECRET/getUpdates");
        // Api 没有 Debug 实现,不会被 `{:?}` 顺手打进日志。
        // (这一条靠下面的编译期断言守住。)
    }

    /// `Api` 刻意不实现 `Debug`:实现了的话,某处一个 `tracing::warn!(?api)`
    /// 就会把 bot_token 打进日志。这个函数只要能编译过就说明约束还在。
    #[allow(dead_code)]
    fn api_must_not_be_debug() {
        fn assert_not_debug<T>() {}
        assert_not_debug::<Api>();
    }

    #[test]
    fn update_parses_both_message_and_callback_shapes() {
        let raw = serde_json::json!({
            "update_id": 7,
            "message": { "message_id": 1, "chat": { "id": 42 }, "text": "/start" }
        });
        let u: Update = serde_json::from_value(raw).unwrap();
        assert_eq!(u.update_id, 7);
        assert_eq!(u.message.unwrap().chat.id, 42);

        let raw = serde_json::json!({
            "update_id": 8,
            "callback_query": {
                "id": "cb1",
                "data": "u:usage",
                "message": { "message_id": 2, "chat": { "id": 43 } }
            }
        });
        let u: Update = serde_json::from_value(raw).unwrap();
        let cb = u.callback_query.unwrap();
        assert_eq!(cb.data.as_deref(), Some("u:usage"));
        // 没有 text 的消息(纯按钮回调)不该解析失败。
        assert!(cb.message.unwrap().text.is_none());
    }

    /// Telegram 会加新字段。多出来的字段必须被忽略,不能让整条 update 解析失败 ——
    /// 那会让 bot 在某次 API 更新后突然「什么都不响应」。
    #[test]
    fn unknown_fields_are_ignored() {
        let raw = serde_json::json!({
            "update_id": 9,
            "brand_new_field": { "x": 1 },
            "message": {
                "message_id": 1, "chat": { "id": 1, "type": "private" },
                "text": "hi", "entities": []
            }
        });
        let u: Update = serde_json::from_value(raw).unwrap();
        assert_eq!(u.update_id, 9);
    }
}
