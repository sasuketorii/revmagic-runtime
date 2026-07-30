//! MCP ツール定義と実行。
//!
//! 変換ロジックは **再実装しない**。`revmagic-core` を single source of truth
//! として呼び出し、CLI (`crates/revmagic-cli`) と同じ振り分け
//! (`route_for_url`) に従う。
//!
//! 公開ツール:
//! - [`url_to_mp3`](call_url_to_mp3): URL / ローカルファイルを MP3 化 (同期完走)。
//! - [`probe_url`](call_probe_url): route 判定 (実 DL しない軽量プローブ)。
//!
//! ## 秘密保護
//! - cookie / referer は引数で受け取るが、応答・ログ・evidence に生で出さない。
//! - 失敗時は core が組み立てた redact 済み message を返す。
//! - 進捗は stderr のみ ([`crate::elog`])。stdout の JSON-RPC を壊さない。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use revmagic_core::{
    convert_media_blocking, convert_url_stream_to_mp3_with_cancel, extract_url_to_mp3,
    redact_url_queries, route_for_url, ConversionRequest, ConversionSummary,
    FallbackBinaryResolver, MediaRoute, Progress, YtDlpError, YtDlpErrorKind,
};
use serde::Serialize;
use serde_json::{json, Value};
use url::Url;

/// ツール実行コンテキスト。バイナリ resolver を保持する。
///
/// バイナリ解決を保持する最小実行コンテキスト。
pub struct ToolContext {
    resolver: FallbackBinaryResolver,
}

impl ToolContext {
    /// 同梱 `binaries/` を best-effort で優先する resolver を構築する。
    ///
    /// 実行ファイル隣接のバイナリを候補にする。
    /// 見つからなくても core の PATH / Homebrew fallback が効くので失敗しない。
    pub fn new() -> Self {
        let mut resolver = FallbackBinaryResolver::new();

        // 実行ファイル隣接の `binaries/` は **working slot** (その場で動く前提)。
        // 極小 shebang の yt-dlp shim でも受理する (F1 指摘1: working は shim 拒否しない)。
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                resolver = resolver.with_local_dir(dir.to_path_buf());
            }
        }

        Self { resolver }
    }
}

impl Default for ToolContext {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tool definitions (tools/list)
// ---------------------------------------------------------------------------

/// `tools/list` が返すツール定義一覧 (name / description / inputSchema)。
pub fn tool_definitions() -> Vec<Value> {
    vec![url_to_mp3_definition(), probe_url_definition()]
}

fn url_to_mp3_definition() -> Value {
    json!({
      "name": "url_to_mp3",
      "description": "URL またはローカルファイルを MP3 へ変換する (同期完走)。\
        ユーザーが『MP3にして』『MP3化』『音声にして』『文字起こし用に音声化』『この動画を音声で』等と言った時に使う。\
        直リンク / HLS は ffmpeg、埋め込み (Loom 等) は yt-dlp 経路で処理する。\
        認証が必要で cookie 未指定の場合は認証エラーを返す。",
      "inputSchema": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "url": {
            "type": "string",
            "description": "変換元 URL (http/https 直リンク・HLS・埋め込みページ) または file:// URL / ローカル絶対パス。"
          },
          "output_dir": {
            "type": "string",
            "description": "出力先ディレクトリ。未指定時は ~/Downloads に保存する (無ければ作成)。明示指定したパスは既存である必要がある。"
          },
          "split_minutes": {
            "type": "integer",
            "minimum": 1,
            "description": "N 分ごとに分割出力する (任意)。"
          },
          "optimize": {
            "type": "boolean",
            "description": "ファイルサイズ優先でエンコードする (任意, 既定 false)。"
          },
          "cookie": {
            "type": "string",
            "description": "HTTP Cookie ヘッダ値 (任意, 案A=引数渡し)。応答・ログに生で出力されない。"
          },
          "referer": {
            "type": "string",
            "description": "HTTP Referer ヘッダ値 (任意)。応答・ログに生で出力されない。"
          }
        },
        "required": ["url"]
      }
    })
}

fn probe_url_definition() -> Value {
    json!({
      "name": "probe_url",
      "description": "URL の route 判定 (direct / extractor) を返す軽量プローブ。\
        実際のダウンロード・変換は行わない。\
        (『MP3にして』系の要求で、まず安全に route 判定したい時に使う)",
      "inputSchema": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "url": {
            "type": "string",
            "description": "判定対象 URL。"
          }
        },
        "required": ["url"]
      }
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// ツール名と引数を受けて core を呼び出す。成功時は structuredContent となる
/// JSON を、失敗時は redact 済みユーザ向け文言を `Err` で返す。
pub fn call_tool(ctx: &ToolContext, name: &str, arguments: &Value) -> Result<Value, String> {
    let args = arguments
        .as_object()
        .ok_or_else(|| "arguments must be an object".to_string())?;

    match name {
        "url_to_mp3" => call_url_to_mp3(ctx, args),
        "probe_url" => call_probe_url(args),
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// probe_url
// ---------------------------------------------------------------------------

fn call_probe_url(args: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let url = require_str(args, "url")?;
    let parsed = Url::parse(url).map_err(|error| format!("URL を解析できませんでした: {error}"))?;

    let route = route_for_url(&parsed);
    let (route_name, handler) = match route {
        MediaRoute::Direct => ("direct", "ffmpeg"),
        MediaRoute::Extractor => ("extractor", "yt-dlp"),
    };

    // 署名付き / 期限付き URL の query/fragment (`?token=...&sig=...`) を応答に
    // 載せない。url は core の共通 sanitizer で `?<redacted>` 化したものだけを返す。
    // route 判定に必要な情報は scheme / host / route / handler / isHls の構造化
    // フィールドで返すため、生の token/sig は不要。
    Ok(json!({
      "url": redact_url_queries(parsed.as_str()),
      "scheme": parsed.scheme(),
      "host": parsed.host_str(),
      "route": route_name,
      "handler": handler,
      "isHls": revmagic_core::is_hls_url(&parsed)
    }))
}

// ---------------------------------------------------------------------------
// url_to_mp3
// ---------------------------------------------------------------------------

/// url_to_mp3 引数を型付き構造体へまとめる。
struct UrlToMp3Params {
    /// `url`。`file://` / 絶対パスはローカル入力として扱う。
    source: String,
    output_dir: PathBuf,
    /// `output_dir` が未指定で `~/Downloads` を既定採用した場合 true。
    /// 既定パスのみ「無ければ作成」、明示パスは「要存在」を維持する。
    output_dir_is_default: bool,
    split_minutes: Option<u64>,
    optimize: bool,
    cookie: Option<String>,
    referer: Option<String>,
}

/// stdout に出す構造化サマリ (CLI の `CliSummary` と同形)。秘密情報は含まない。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolSummary {
    ok: bool,
    source_kind: &'static str,
    #[serde(flatten)]
    summary: ConversionSummary,
}

fn call_url_to_mp3(
    ctx: &ToolContext,
    args: &serde_json::Map<String, Value>,
) -> Result<Value, String> {
    let params = parse_url_to_mp3_params(args)?;
    let summary = run_url_to_mp3(ctx, params)?;
    serde_json::to_value(&summary)
        .map_err(|error| format!("サマリの JSON 化に失敗しました: {error}"))
}

fn parse_url_to_mp3_params(
    args: &serde_json::Map<String, Value>,
) -> Result<UrlToMp3Params, String> {
    let source = require_str(args, "url")?.to_string();

    // output_dir は任意。未指定 or 空文字なら ~/Downloads を既定にする。
    // 既定採用時のみ「無ければ作成」、明示指定は run 時に「要存在」を維持する。
    let (output_dir, output_dir_is_default) = match optional_str(args, "output_dir")? {
        Some(dir) => (PathBuf::from(dir), false),
        None => (default_downloads_dir()?, true),
    };

    let split_minutes = match args.get("split_minutes") {
        Some(Value::Null) | None => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| "split_minutes は正の整数で指定してください。".to_string())?,
        ),
    };

    let optimize = match args.get("optimize") {
        Some(Value::Null) | None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| "optimize は真偽値で指定してください。".to_string())?,
    };

    let cookie = optional_str(args, "cookie")?;
    let referer = optional_str(args, "referer")?;

    Ok(UrlToMp3Params {
        source,
        output_dir,
        output_dir_is_default,
        split_minutes,
        optimize,
        cookie,
        referer,
    })
}

/// `output_dir` 未指定時の既定保存先 `<HOME>/Downloads` を返す。
///
/// `HOME` が取得できない (未設定 / 非UTF-8) 場合は明確なエラーを返す。
/// 既定パスは run 時に存在しなければ作成する (明示指定は要存在のまま)。
fn default_downloads_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| {
        "保存先 (output_dir) が未指定で、既定の ~/Downloads も解決できません \
     (環境変数 HOME が未設定)。output_dir を明示してください。"
            .to_string()
    })?;
    if home.trim().is_empty() {
        return Err(
            "保存先 (output_dir) が未指定で、既定の ~/Downloads も解決できません \
       (環境変数 HOME が空)。output_dir を明示してください。"
                .to_string(),
        );
    }
    Ok(PathBuf::from(home).join("Downloads"))
}

fn run_url_to_mp3(ctx: &ToolContext, params: UrlToMp3Params) -> Result<ToolSummary, String> {
    let resolver = &ctx.resolver;
    let progress = make_progress_logger();

    // 既定パス (~/Downloads) は無ければ作成する。明示指定のパスは
    // タイポで勝手にディレクトリを作らないよう「要存在」を維持する。
    if params.output_dir_is_default {
        std::fs::create_dir_all(&params.output_dir).map_err(|error| {
            format!(
                "既定の保存先フォルダを作成できませんでした ({}): {error}",
                params.output_dir.display()
            )
        })?;
    } else if !params.output_dir.exists() {
        return Err(format!(
            "保存先フォルダが見つかりません: {}",
            params.output_dir.display()
        ));
    }

    // file:// URL またはローカル絶対パスはローカル入力として扱う。
    if let Some(local) = as_local_path(&params.source) {
        if !local.exists() {
            return Err(format!("入力ファイルが見つかりません: {}", local.display()));
        }
        let summary = convert_local_to_mp3(
            resolver,
            &local,
            &params.output_dir,
            params.split_minutes,
            params.optimize,
            &progress,
        )?;
        return Ok(ToolSummary {
            ok: true,
            source_kind: "local",
            summary,
        });
    }

    let parsed = Url::parse(&params.source)
        .map_err(|error| format!("URL を解析できませんでした: {error}"))?;

    match route_for_url(&parsed) {
        MediaRoute::Direct => {
            // Speed-2/Speed-4 (GUI と同じ経路に統一): 「全 DL → 一時ファイル →
            // 逐次変換」をやめ、ffmpeg に URL (直リンク / HLS いずれも) を直接入力して
            // ストリーム変換する。DL とエンコードが重なり一時ファイルが不要になる分、
            // GUI 経由と同じ体感速度になる (以前は MCP だけ旧来の2段階のままだった)。
            crate::elog("直リンク / HLS をストリーム変換します (cookie/referer は redacted)。");
            let summary = convert_url_stream_to_mp3_with_cancel(
                resolver,
                &params.source,
                &params.output_dir,
                params.split_minutes,
                params.optimize,
                params.cookie.as_deref(),
                params.referer.as_deref(),
                None, // suggested_title: MCP には sniff 由来のタイトル候補が無い
                &progress,
                None, // cancel: MCP ツール呼び出しは同期完走のみ (キャンセル機構なし)
            )
            .map_err(manual_error_guidance)?;
            Ok(ToolSummary {
                ok: true,
                source_kind: "url-direct",
                summary,
            })
        }
        MediaRoute::Extractor => {
            crate::elog("埋め込み動画を yt-dlp で解決します (cookie/referer は redacted)。");
            let summary = extract_via_ytdlp_to_summary(resolver, &parsed, &params, &progress)
                .map_err(manual_auth_guidance)?;
            Ok(ToolSummary {
                ok: true,
                source_kind: "url-ytdlp",
                summary,
            })
        }
    }
}

/// job folder 隔離・分割時の `segment_mp3_copy` (`-c copy`) 経路・raw job folder の
/// 削除・正しい `ConversionSummary` (`output_folder` = 実 job folder) を core 内で
/// 一貫して行い、バグ#2 (出力先にフォルダ + 生 MP3 / temp 残置) を MCP 経路でも解消する。
///
/// `cookie` は引数のヘッダ文字列として渡す。core はこれを 0600 Netscape jar に変換し `--cookies` で
/// yt-dlp へ渡し、完了後 (Drop) に削除する。MCP 側はファイルに書かない。
///
/// 戻り値は種別付き [`YtDlpError`]。認証または抽出ブロック時は呼び出し側が
/// 明示cookie/refererの再試行を案内する。
fn extract_via_ytdlp_to_summary(
    resolver: &FallbackBinaryResolver,
    url: &Url,
    params: &UrlToMp3Params,
    progress: &(impl Fn(&Progress) -> Result<(), String> + Send + Sync),
) -> Result<ConversionSummary, YtDlpError> {
    extract_url_to_mp3(
        resolver,
        url.as_str(),
        &params.output_dir,
        params.split_minutes,
        params.optimize,
        params.cookie.as_deref(),
        params.referer.as_deref(),
        progress,
    )
}

fn manual_auth_guidance(error: YtDlpError) -> String {
    match error.kind() {
        YtDlpErrorKind::AuthRequired | YtDlpErrorKind::ExtractionBlocked => format!(
            "{error} 認証またはサイト制約があるURLは、アクセス権のあるブラウザから取得したcookieとrefererを明示して再試行してください。"
        ),
        _ => error.to_string(),
    }
}

fn manual_error_guidance(error: String) -> String {
    if error.contains("[AUTH_REQUIRED]") {
        format!("{error} アクセス権のあるブラウザから取得したcookieとrefererを明示して再試行してください。")
    } else {
        error
    }
}

fn as_local_path(source: &str) -> Option<PathBuf> {
    if let Ok(url) = Url::parse(source) {
        return match url.scheme() {
            "file" => url.to_file_path().ok(),
            "http" | "https" => None,
            _ => None,
        };
    }
    Some(PathBuf::from(source))
}

fn convert_local_to_mp3(
    resolver: &FallbackBinaryResolver,
    input: &Path,
    output_dir: &Path,
    split_minutes: Option<u64>,
    optimize: bool,
    progress: &(impl Fn(&Progress) -> Result<(), String> + Send + Sync),
) -> Result<ConversionSummary, String> {
    convert_media_blocking(
        resolver,
        ConversionRequest {
            input_path: input.to_string_lossy().to_string(),
            output_directory: output_dir.to_string_lossy().to_string(),
            split_minutes,
            optimize_for_size: optimize,
        },
        progress,
    )
}

/// core の async ダウンロードを tokio ランタイムで駆動し一時ファイルパスを返す。
/// 進捗を stderr へ出すコールバック。`Progress` は cookie/referer/token を含まない。
fn make_progress_logger() -> impl Fn(&Progress) -> Result<(), String> + Send + Sync {
    let last = Mutex::new((0u8, String::new()));
    move |progress: &Progress| {
        let mut guard = last
            .lock()
            .map_err(|_| "進捗ロックの取得に失敗".to_string())?;
        let changed = progress.percent > guard.0 || progress.step != guard.1;
        if changed {
            *guard = (progress.percent, progress.step.clone());
            crate::elog(&format!(
                "{:>3}% {} - {}",
                progress.percent, progress.step, progress.detail
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn require_str<'a>(args: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("引数 '{key}' (文字列) が必要です。"))
}

/// 任意文字列引数。存在すれば文字列であることを検証し、空 / 空白は `None` に倒す。
fn optional_str(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match args.get(key) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            Ok(if trimmed.is_empty() {
                None
            } else {
                Some(s.clone())
            })
        }
        Some(_) => Err(format!("引数 '{key}' は文字列で指定してください。")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_carry_required_input_schema() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 2);
        let url_tool = &defs[0];
        assert_eq!(url_tool["name"], "url_to_mp3");
        let required = url_tool["inputSchema"]["required"].as_array().unwrap();
        let req: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        // output_dir は任意化済み。required は url のみ。
        assert_eq!(req, vec!["url"]);
        assert!(!req.contains(&"output_dir"));

        let probe = &defs[1];
        assert_eq!(probe["name"], "probe_url");
        assert_eq!(probe["inputSchema"]["required"][0], "url");
    }

    #[test]
    fn probe_direct_media() {
        let out = call_probe_url(
            serde_json::json!({ "url": "https://e.com/clip.mp4" })
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["route"], "direct");
        assert_eq!(out["handler"], "ffmpeg");
        assert_eq!(out["isHls"], false);
    }

    #[test]
    fn probe_hls_is_direct() {
        let out = call_probe_url(
            serde_json::json!({ "url": "https://e.com/master.m3u8?t=1" })
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["route"], "direct");
        assert_eq!(out["isHls"], true);
    }

    #[test]
    fn probe_loom_is_extractor() {
        let out = call_probe_url(
            serde_json::json!({ "url": "https://www.loom.com/share/abcdef123" })
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["route"], "extractor");
        assert_eq!(out["handler"], "yt-dlp");
    }

    #[test]
    fn probe_url_redacts_query_token_in_structured_and_url() {
        // 署名付き URL の token/sig は probe_url の応答 (structuredContent.url) に
        // 生で残ってはならない。route 判定情報は構造化フィールドで返る。
        let out = call_probe_url(
            serde_json::json!({
              "url": "https://example.com/clip.mp4?token=REV_MAGIC_URL_TOKEN_SENTINEL&sig=ABC"
            })
            .as_object()
            .unwrap(),
        )
        .unwrap();
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            !serialized.contains("REV_MAGIC_URL_TOKEN_SENTINEL"),
            "token sentinel leaked into probe_url output: {serialized}"
        );
        assert!(
            !serialized.contains("sig=ABC"),
            "sig leaked into probe_url output: {serialized}"
        );
        // 構造化フィールドは redact 後も route 判定を返す。
        assert_eq!(out["route"], "direct");
        assert_eq!(out["handler"], "ffmpeg");
        assert_eq!(out["host"], "example.com");
        assert_eq!(out["url"], "https://example.com/clip.mp4?<redacted>");
    }

    #[test]
    fn probe_rejects_bad_url() {
        let err = call_probe_url(
            serde_json::json!({ "url": "not a url" })
                .as_object()
                .unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("解析"));
    }

    #[test]
    fn require_str_rejects_missing_and_blank() {
        let m = serde_json::json!({ "a": "  ", "b": "ok" });
        let m = m.as_object().unwrap();
        assert!(require_str(m, "a").is_err());
        assert!(require_str(m, "missing").is_err());
        assert_eq!(require_str(m, "b").unwrap(), "ok");
    }

    #[test]
    fn optional_str_blank_becomes_none() {
        let m = serde_json::json!({ "cookie": "   ", "referer": "https://r/", "num": 1 });
        let m = m.as_object().unwrap();
        assert_eq!(optional_str(m, "cookie").unwrap(), None);
        assert_eq!(optional_str(m, "missing").unwrap(), None);
        assert_eq!(
            optional_str(m, "referer").unwrap().as_deref(),
            Some("https://r/")
        );
        assert!(optional_str(m, "num").is_err());
    }

    #[test]
    fn as_local_path_classifies_sources() {
        assert!(as_local_path("https://e.com/x.mp4").is_none());
        assert!(as_local_path("http://e.com/x").is_none());
        assert_eq!(
            as_local_path("/tmp/x.mp4"),
            Some(PathBuf::from("/tmp/x.mp4"))
        );
        // file:// URL は絶対パスへ。
        let f = as_local_path("file:///tmp/clip.mp4").unwrap();
        assert_eq!(f, PathBuf::from("/tmp/clip.mp4"));
    }

    #[test]
    fn parse_params_validates_types() {
        let bad_split = serde_json::json!({
          "url": "https://e.com/x.mp4", "output_dir": "/tmp", "split_minutes": "five"
        });
        assert!(parse_url_to_mp3_params(bad_split.as_object().unwrap()).is_err());

        let bad_opt = serde_json::json!({
          "url": "https://e.com/x.mp4", "output_dir": "/tmp", "optimize": "yes"
        });
        assert!(parse_url_to_mp3_params(bad_opt.as_object().unwrap()).is_err());

        let ok = serde_json::json!({
          "url": "https://e.com/x.mp4", "output_dir": "/tmp",
          "split_minutes": 5, "optimize": true, "cookie": "a=b", "referer": "https://r/"
        });
        let p = parse_url_to_mp3_params(ok.as_object().unwrap()).unwrap();
        assert_eq!(p.split_minutes, Some(5));
        assert!(p.optimize);
        assert_eq!(p.cookie.as_deref(), Some("a=b"));
        // 明示 output_dir は既定扱いにならない。
        assert!(!p.output_dir_is_default);
        assert_eq!(p.output_dir, PathBuf::from("/tmp"));
    }

    #[test]
    fn parse_params_defaults_output_dir_to_home_downloads() {
        // output_dir 未指定 -> <HOME>/Downloads を既定採用し is_default=true。
        // HOME はテスト内で一時上書きし、本物の ~/Downloads を汚さない。
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", "/tmp/rmhome-unit");

        let no_dir = serde_json::json!({ "url": "https://e.com/x.mp4" });
        let p = parse_url_to_mp3_params(no_dir.as_object().unwrap()).unwrap();
        assert!(p.output_dir_is_default);
        assert_eq!(p.output_dir, PathBuf::from("/tmp/rmhome-unit/Downloads"));

        // 空文字も未指定と同じく既定へ倒す。
        let empty_dir = serde_json::json!({ "url": "https://e.com/x.mp4", "output_dir": "  " });
        let p2 = parse_url_to_mp3_params(empty_dir.as_object().unwrap()).unwrap();
        assert!(p2.output_dir_is_default);
        assert_eq!(p2.output_dir, PathBuf::from("/tmp/rmhome-unit/Downloads"));

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn call_tool_rejects_non_object_arguments() {
        let ctx = ToolContext::new();
        let err = call_tool(&ctx, "probe_url", &serde_json::json!([])).unwrap_err();
        assert!(err.contains("must be an object"));
    }

    #[test]
    fn call_tool_unknown_name() {
        let ctx = ToolContext::new();
        let err = call_tool(&ctx, "ghost", &serde_json::json!({})).unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[test]
    fn direct_auth_error_includes_manual_cookie_and_referer_guidance() {
        let error = manual_error_guidance("[AUTH_REQUIRED] HTTP 403".to_string());
        assert!(error.contains("cookie"));
        assert!(error.contains("referer"));
    }
}
