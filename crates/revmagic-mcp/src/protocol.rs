//! MCP JSON-RPC 2.0 protocol over stdio.
//!
//! 外部 SDK に依存せず、手書き dispatch と改行区切り JSON フレーミングを使う。
//! ただし最新 MCP 仕様に寄せて以下を追加している:
//!
//! - **protocolVersion ネゴシエート**: クライアント (`initialize.params.protocolVersion`)
//!   が送ってきた版がサーバ既知の版なら **その版をそのまま echo** し、未知なら
//!   サーバが推奨する最新版 ([`LATEST_PROTOCOL_VERSION`]) を返す。これにより
//!   Codex / Claude Code 双方の互換を最優先しつつ、版を明示しない古い実装にも
//!   フォールバックする。
//! - **structuredContent**: `tools/call` の成功結果は `content:[{type:"text",...}]`
//!   (後方互換のテキスト) に加え、構造化結果 `structuredContent` を併せて返す。
//! - **isError**: ツール実行失敗は protocol error ではなく tool result
//!   (`isError:true` + テキスト) として返す。JSON-RPC レベルのエラー (不正な
//!   リクエスト・未知メソッド) のみ JSON-RPC error object を返す。
//!
//! ## stdout 規約
//! stdout には **MCP JSON-RPC メッセージのみ** を書く。ログ・進捗・診断はすべて
//! stderr へ出す ([`crate::elog`])。これを破ると Codex / Claude Code 側の
//! フレーミングが壊れる。

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::{self, ToolContext};

/// `initialize` で返すサーバ名。
const SERVER_NAME: &str = "revmagic-mcp";
/// `initialize` で返すサーバ版。
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// サーバが推奨する最新 MCP プロトコル版。
///
/// クライアントが未知 / 未指定の版を送ってきた場合のフォールバック値。
/// 既知版を送ってきた場合はその版を echo して互換を最優先する。
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// サーバが理解できる protocol 版 (新しい順)。クライアントが送ってきた版が
/// この一覧にあれば echo する。`2024-11-05` は初期版で、
/// 古いクライアントとの互換のため受理する。
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 request (incoming).
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    /// Must be "2.0".
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// Request identifier (notifications では欠落)。
    pub id: Option<Value>,
    /// Method name.
    pub method: String,
    /// Optional parameters.
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response (outgoing).
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Standard error codes
// ---------------------------------------------------------------------------

const PARSE_ERROR: i64 = -32700;
const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC 1 行 (1 メッセージ) の最大バイト数 (B4)。
///
/// `BufRead::lines()` / `read_line` は改行までを無制限にメモリへ読み込むため、
/// 巨大な 1 行 (改行なしの大量バイト) を送られると OOM し得る。これを防ぐため、
/// 行をこの上限まで読み、超過したら明示エラーにする。url_to_mp3 / probe_url の
/// 正当なリクエストは数 KB 程度なので、8 MiB は実用上十分に広い。
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// stdin から改行区切り JSON-RPC を読み、dispatch して stdout へ応答を書く。
///
/// stdin が閉じるか致命的 I/O エラーが起きるまでブロックする。リクエスト単位の
/// 回復可能なエラーは JSON-RPC error / tool error 応答として返し、ループは継続する。
pub fn run_stdio_server(ctx: &ToolContext) -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let reader = stdin.lock();
    let mut writer = stdout.lock();
    run_loop(ctx, reader, &mut writer)
}

/// テスト可能な本体: 任意の `BufRead` / `Write` で駆動する。
pub fn run_loop<R: BufRead, W: Write>(
    ctx: &ToolContext,
    mut reader: R,
    writer: &mut W,
) -> Result<(), String> {
    loop {
        let mut buf: Vec<u8> = Vec::new();
        // B4: 行サイズを MAX_LINE_BYTES で上限化して読む。改行を含まない巨大入力で
        // メモリを無制限に消費する (OOM) のを防ぐ。EOF なら終了、超過は行を破棄して
        // parse error 応答を返しループ継続 (DoS では落とさない)。
        match read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES) {
            ReadOutcome::Eof => break,
            ReadOutcome::Line => {}
            ReadOutcome::TooLong => {
                crate::elog("JSON-RPC 行がサイズ上限を超えました。破棄します。");
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: PARSE_ERROR,
                        message: "request line exceeds maximum allowed size".to_string(),
                        data: None,
                    }),
                };
                write_response(writer, &resp)?;
                continue;
            }
            ReadOutcome::Io(e) => return Err(format!("stdin read error: {e}")),
        }

        // 不正な UTF-8 は parse error として扱い、ループは継続する。
        let line = match std::str::from_utf8(&buf) {
            Ok(s) => s,
            Err(_) => {
                crate::elog("JSON-RPC 行が不正な UTF-8 です。破棄します。");
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: PARSE_ERROR,
                        message: "request line is not valid UTF-8".to_string(),
                        data: None,
                    }),
                };
                write_response(writer, &resp)?;
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                crate::elog(&format!("invalid JSON-RPC request: {e}"));
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: PARSE_ERROR,
                        message: format!("parse error: {e}"),
                        data: None,
                    }),
                };
                write_response(writer, &resp)?;
                continue;
            }
        };

        // Notifications (id 無し) は応答しない。
        if request.id.is_none() {
            crate::elog(&format!("notification: {}", request.method));
            continue;
        }

        let response = dispatch(ctx, &request);
        write_response(writer, &response)?;
    }

    Ok(())
}

/// [`read_line_capped`] の結果。
enum ReadOutcome {
    /// 1 行を `buf` に読み込んだ (末尾の改行は除去済み)。
    Line,
    /// 入力が尽きた (これ以上の行は無い)。
    Eof,
    /// 行が `max_bytes` を超えた。現在の行に属する残りバイトは次の改行まで
    /// 読み捨て済み (= ストリームは次の行の先頭に同期している)。
    TooLong,
    /// I/O エラー。
    Io(io::Error),
}

/// 1 行 (改行区切り) を **最大 `max_bytes` まで** 読む (B4: OOM 防御)。
///
/// `BufRead::read_until` と異なり、改行が現れる前に `max_bytes` を超えたら読み込みを
/// 止め、行末 (改行) まで残りを **破棄** してから [`ReadOutcome::TooLong`] を返す。これに
/// より、改行なしの巨大入力でメモリを無制限に確保することを防ぎつつ、後続の正当な行と
/// 同期を保てる。`buf` には末尾の `\n` を含めない (呼び出し側の `trim` 前提に合わせる)。
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> ReadOutcome {
    buf.clear();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                // EOF: それまでに読んだ分があれば 1 行として処理し、無ければ終了。
                return if buf.is_empty() {
                    ReadOutcome::Eof
                } else {
                    ReadOutcome::Line
                };
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return ReadOutcome::Line;
                }
                if buf.len() >= max_bytes {
                    // 上限超過: この行の残り (次の改行 or EOF まで) を読み捨て、同期を保つ。
                    loop {
                        match reader.read(&mut byte) {
                            Ok(0) => return ReadOutcome::TooLong,
                            Ok(_) if byte[0] == b'\n' => return ReadOutcome::TooLong,
                            Ok(_) => continue,
                            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => return ReadOutcome::Io(e),
                        }
                    }
                }
                buf.push(byte[0]);
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return ReadOutcome::Io(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn dispatch(ctx: &ToolContext, req: &JsonRpcRequest) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => handle_initialize(req),
        "tools/list" => handle_tools_list(req),
        "tools/call" => handle_tools_call(ctx, req),
        "ping" => ok_response(req.id.clone(), Value::Object(serde_json::Map::new())),
        // notifications/initialized 等が id 付きで来た場合は空 result で ack。
        method if method.starts_with("notifications/") => {
            ok_response(req.id.clone(), Value::Object(serde_json::Map::new()))
        }
        other => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id.clone(),
            result: None,
            error: Some(JsonRpcError {
                code: METHOD_NOT_FOUND,
                message: format!("method not found: {other}"),
                data: None,
            }),
        },
    }
}

// ---------------------------------------------------------------------------
// Method handlers
// ---------------------------------------------------------------------------

/// protocolVersion をネゴシエートする。
///
/// クライアントが送った版が既知なら echo、未知 / 未指定なら最新版を返す。
fn negotiate_protocol_version(params: Option<&Value>) -> String {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str());

    match requested {
        Some(ver) if SUPPORTED_PROTOCOL_VERSIONS.contains(&ver) => ver.to_string(),
        _ => LATEST_PROTOCOL_VERSION.to_string(),
    }
}

fn handle_initialize(req: &JsonRpcRequest) -> JsonRpcResponse {
    let protocol_version = negotiate_protocol_version(req.params.as_ref());

    let result = serde_json::json!({
      "protocolVersion": protocol_version,
      "capabilities": {
        "tools": { "listChanged": false }
      },
      "serverInfo": {
        "name": SERVER_NAME,
        "version": SERVER_VERSION
      }
    });

    ok_response(req.id.clone(), result)
}

fn handle_tools_list(req: &JsonRpcRequest) -> JsonRpcResponse {
    let tools = tools::tool_definitions();
    let result = serde_json::json!({ "tools": tools });
    ok_response(req.id.clone(), result)
}

fn handle_tools_call(ctx: &ToolContext, req: &JsonRpcRequest) -> JsonRpcResponse {
    let params = req.params.as_ref().and_then(|p| p.as_object());

    let tool_name = match params.and_then(|p| p.get("name")).and_then(|v| v.as_str()) {
        Some(name) => name,
        None => {
            return JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id.clone(),
                result: None,
                error: Some(JsonRpcError {
                    code: INVALID_PARAMS,
                    message: "tools/call requires a string 'name'".to_string(),
                    data: None,
                }),
            };
        }
    };

    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    match tools::call_tool(ctx, tool_name, &arguments) {
        Ok(structured) => {
            // 最新仕様: structuredContent + 後方互換の text content。
            let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".to_string());
            let result = serde_json::json!({
              "content": [{ "type": "text", "text": text }],
              "structuredContent": structured,
              "isError": false
            });
            ok_response(req.id.clone(), result)
        }
        Err(message) => {
            // ツール実行失敗は protocol error ではなく tool result error。
            //
            // MCP 境界の共通防御層: tool error message を stderr ログ / tool result
            // text に出す **直前** で、署名付き URL の query/fragment (`?token=...&sig=...`)
            // を core の sanitizer で一律 redact する。direct download 経路の reqwest
            // エラーは full URL を文字列化し得るため (yt-dlp 経路の cookie redaction とは
            // 別経路)、ここで漏れを断つ。実際の DL に渡す URL は core 内で生のまま使われ、
            // この sanitize は「外へ出す文字列」だけに効く。
            let redacted = revmagic_core::redact_url_queries(&message);
            crate::elog(&format!("tool '{tool_name}' error: {redacted}"));
            let result = serde_json::json!({
              "content": [{ "type": "text", "text": redacted }],
              "isError": true
            });
            ok_response(req.id.clone(), result)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_response(id: Option<Value>, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    }
}

fn write_response<W: Write>(writer: &mut W, resp: &JsonRpcResponse) -> Result<(), String> {
    let json =
        serde_json::to_string(resp).map_err(|e| format!("failed to serialize response: {e}"))?;
    writeln!(writer, "{json}").map_err(|e| format!("stdout write error: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("stdout flush error: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::new()
    }

    fn request(method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: method.to_string(),
            params: Some(params),
        }
    }

    #[test]
    fn initialize_echoes_known_protocol_version() {
        let req = request(
            "initialize",
            serde_json::json!({ "protocolVersion": "2025-03-26" }),
        );
        let resp = handle_initialize(&req);
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_latest_for_unknown_version() {
        let req = request(
            "initialize",
            serde_json::json!({ "protocolVersion": "1999-01-01" }),
        );
        let resp = handle_initialize(&req);
        assert_eq!(
            resp.result.unwrap()["protocolVersion"],
            LATEST_PROTOCOL_VERSION
        );
    }

    #[test]
    fn initialize_falls_back_when_version_absent() {
        let req = request("initialize", serde_json::json!({}));
        let resp = handle_initialize(&req);
        assert_eq!(
            resp.result.unwrap()["protocolVersion"],
            LATEST_PROTOCOL_VERSION
        );
    }

    #[test]
    fn tools_list_returns_both_tools_with_schema() {
        let resp = handle_tools_list(&request("tools/list", serde_json::json!({})));
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"url_to_mp3"));
        assert!(names.contains(&"probe_url"));
        for t in tools {
            assert!(
                t["inputSchema"].is_object(),
                "tool must carry inputSchema: {t:?}"
            );
        }
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let resp = dispatch(&ctx(), &request("does/not/exist", serde_json::json!({})));
        assert!(resp.result.is_none());
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn tools_call_missing_name_is_invalid_params() {
        let resp = handle_tools_call(&ctx(), &request("tools/call", serde_json::json!({})));
        assert!(resp.result.is_none());
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }

    #[test]
    fn tools_call_unknown_tool_is_tool_error_not_protocol_error() {
        let resp = handle_tools_call(
            &ctx(),
            &request(
                "tools/call",
                serde_json::json!({ "name": "nope", "arguments": {} }),
            ),
        );
        // protocol error ではなく tool result error。
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn probe_url_returns_structured_content() {
        let resp = handle_tools_call(
            &ctx(),
            &request(
                "tools/call",
                serde_json::json!({ "name": "probe_url", "arguments": { "url": "https://e.com/clip.mp4" } }),
            ),
        );
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["route"], "direct");
        // text content も併せて返る (後方互換)。
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("direct"));
    }

    #[test]
    fn run_loop_handles_invalid_json_then_continues() {
        let input = b"not json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let mut out: Vec<u8> = Vec::new();
        run_loop(&ctx(), &input[..], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one error line + one tools/list line");
        let parse_err: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parse_err["error"]["code"], PARSE_ERROR);
        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert!(list["result"]["tools"].is_array());
    }

    #[test]
    fn read_line_capped_returns_too_long_then_resyncs_to_next_line() {
        // 上限を超える 1 行のあと、正当な行が続く。上限超過行は TooLong、
        // 次の行は正しく読めること (= 同期回復) を確認する。
        let oversize = "x".repeat(50);
        let input = format!("{oversize}\n{{\"ok\":1}}\n");
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let mut buf = Vec::new();
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf, 16),
            ReadOutcome::TooLong
        ));
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf, 16),
            ReadOutcome::Line
        ));
        assert_eq!(String::from_utf8(buf.clone()).unwrap(), "{\"ok\":1}");
    }

    #[test]
    fn run_loop_rejects_oversize_line_then_continues() {
        // 巨大 1 行 (改行なしで上限超過) -> parse error 応答。その後の正当な
        // tools/list は通常どおり処理される (DoS でループを落とさない)。
        let huge = "A".repeat(MAX_LINE_BYTES + 1024);
        let input = format!("{huge}\n{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\"}}\n");
        let mut out: Vec<u8> = Vec::new();
        run_loop(&ctx(), input.as_bytes(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "one oversize-error line + one tools/list line"
        );
        let err: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(err["error"]["code"], PARSE_ERROR);
        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert!(list["result"]["tools"].is_array());
    }

    #[test]
    fn notifications_without_id_produce_no_response() {
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut out: Vec<u8> = Vec::new();
        run_loop(&ctx(), &input[..], &mut out).unwrap();
        assert!(out.is_empty(), "notifications must not be answered");
    }

    #[test]
    fn write_response_emits_single_json_line() {
        let resp = ok_response(Some(Value::from(1)), Value::Null);
        let mut buf: Vec<u8> = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.ends_with('\n'));
        assert_eq!(output.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
    }
}
