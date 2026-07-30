//! revmagic-mcp
//!
//! stdio MCP サーバ。`revmagic-core` を single source
//! of truth として呼び出し、URL / ローカルファイルを MP3 化する MCP ツールを
//! Codex / Claude Code 等から呼べる形で公開する。
//!
//! 認証が必要なURLでは、cookie / referer をMCPツール引数で明示する。
//!
//! ## 出力規約 (厳守)
//! - **stdout には MCP JSON-RPC メッセージのみ**を書く ([`protocol`])。
//! - ログ・進捗・診断はすべて stderr ([`elog`])。
//! - cookie / referer / token を JSON-RPC 応答・stderr ログに生で出さない。
//!   失敗時は core が組み立てた redact 済み message を返す。
//!
//! プロトコルのフレーミングは改行区切り JSON である。

use std::process::ExitCode;

mod protocol;
mod tools;

use tools::ToolContext;

fn main() -> ExitCode {
    let ctx = ToolContext::new();

    elog(&format!(
        "revmagic-mcp v{} 起動 (stdio JSON-RPC, 案A=cookie 引数渡し)",
        env!("CARGO_PKG_VERSION")
    ));

    match protocol::run_stdio_server(&ctx) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            elog(&format!("致命的エラー: {error}"));
            ExitCode::FAILURE
        }
    }
}

/// stderr へログを出す。stdout は JSON-RPC 専用なので決して使わない。
///
/// 渡す文言は cookie / referer / token などの秘密情報を含んではならない
/// (core が組み立てた redact 済み Progress / error 文言のみを渡す)。
pub fn elog(message: &str) {
    eprintln!("[revmagic-mcp] {message}");
}
