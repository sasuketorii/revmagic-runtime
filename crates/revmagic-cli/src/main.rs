//! revmagic-cli
//!
//! standalone 非依存の CLI。`revmagic-core` を single source of truth
//! として呼び出し、ローカルファイルや直リンク / HLS URL を MP3 へ変換する。
//!
//! 設計契約:
//! - **stdout は JSON summary のみ**。後続 MCP がそのままパースできるよう、
//!   変換結果 (出力パス / ファイル一覧 / 長さ等) を構造化 JSON で 1 度だけ出す。
//! - **進捗・ログは stderr**。cookie / referer / token などの秘密情報は生で
//!   出さない (redacted)。
//! - バイナリ解決は `AppHandle` 非依存。core の [`FallbackBinaryResolver`]
//!   (PATH / Homebrew fallback、任意で同梱 `binaries/` を優先) で ffmpeg /
//!   ffprobe / yt-dlp を解決する。
//! - URL は core の振り分け (`route_for_url`) に従い、直リンク / HLS は従来の
//!   `download_media` -> `convert_media_blocking`、埋め込み (Loom 等) は
//!   `extract_audio_via_ytdlp` 経路を使う。振り分けロジックは core に集約。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use clap::{Parser, Subcommand};
use revmagic_core::{
    convert_media_blocking, convert_url_stream_to_mp3_with_cancel, extract_url_to_mp3,
    route_for_url, ConversionRequest, ConversionSummary, FallbackBinaryResolver, MediaRoute,
    Progress,
};
use serde::Serialize;
use url::Url;

/// RevMagic CLI: ローカルファイル / 直リンク / HLS を MP3 化する。
#[derive(Debug, Parser)]
#[command(name = "revmagic-cli", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// 入力 (URL またはローカルファイル) を MP3 へ変換する。
    UrlToMp3(UrlToMp3Args),
}

#[derive(Debug, Parser)]
struct UrlToMp3Args {
    /// 変換元 URL (直リンク .mp4/.mov/.m3u8 等)。`--input` と排他。
    #[arg(long, conflicts_with = "input")]
    url: Option<String>,

    /// 変換元ローカルファイルパス。`--url` と排他。
    #[arg(long, conflicts_with = "url")]
    input: Option<PathBuf>,

    /// 出力先ディレクトリ (任意)。未指定時は ~/Downloads に保存する (無ければ作成)。
    /// 明示指定したパスは既存である必要がある。
    #[arg(long)]
    output_dir: Option<PathBuf>,

    /// HTTP Cookie ヘッダ (任意, 案A=引数渡し)。ログ / stdout には出さない。
    #[arg(long)]
    cookie: Option<String>,

    /// HTTP Referer ヘッダ (任意)。ログ / stdout には出さない。
    #[arg(long)]
    referer: Option<String>,

    /// N 分ごとに分割出力する (任意, 1..=10080 分)。
    /// A14: clap 段で範囲検証し、0 / 過大値 (`* 60` オーバーフロー要因) を弾く。
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=10080))]
    split_minutes: Option<u64>,

    /// ファイルサイズ優先でエンコードする (任意)。
    #[arg(long)]
    optimize: bool,
}

/// stdout に出す構造化サマリ。
///
/// `ConversionSummary` を内包しつつ、入力種別 (`source_kind`) と
/// `ok` フラグを付ける。秘密情報 (cookie/referer/token) は一切含めない。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CliSummary {
    ok: bool,
    source_kind: &'static str,
    #[serde(flatten)]
    summary: ConversionSummary,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::UrlToMp3(args) => match run_url_to_mp3(args) {
            Ok(summary) => {
                // stdout は JSON summary のみ。
                match serde_json::to_string_pretty(&summary) {
                    Ok(json) => {
                        println!("{json}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        elog(&format!("サマリの JSON 化に失敗しました: {error}"));
                        ExitCode::FAILURE
                    }
                }
            }
            Err(error) => {
                // エラーも stderr のみ。stdout は汚さない (空のまま) ので、
                // パイプ先の `jq` 等は空入力で失敗し、CLI の非0終了で検知できる。
                elog(&format!("エラー: {error}"));
                ExitCode::FAILURE
            }
        },
    }
}

/// stderr へ redacted ログを出す。秘密情報を渡してはならない。
fn elog(message: &str) {
    eprintln!("[revmagic-cli] {message}");
}

/// 進捗を stderr へ出すコールバックを作る。
///
/// `Progress` は core が組み立てる文言のみで cookie/referer/token を含まない。
/// 過剰出力を避けるため、percent が前回から進んだ時 / step が変わった時だけ出す。
fn make_progress_logger() -> impl Fn(&Progress) -> Result<(), String> + Send + Sync {
    let last = Mutex::new((0u8, String::new()));
    move |progress: &Progress| {
        // A17: Mutex が poison (別スレッドが保持中に panic) しても進捗を Err にして変換を
        // 中断しない。poison 時は into_inner() で内側の値を回復して継続する。進捗ログの
        // 状態は単なる重複抑制用なので、poison していても安全に使える。
        let mut guard = last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = progress.percent > guard.0 || progress.step != guard.1;
        if changed {
            *guard = (progress.percent, progress.step.clone());
            eprintln!(
                "[revmagic-cli] {:>3}% {} - {}",
                progress.percent, progress.step, progress.detail
            );
        }
        Ok(())
    }
}

/// 同梱 `binaries/` を best-effort で優先する resolver を作る。
///
/// CLI バイナリ位置からの相対を候補にする。
/// 見つからなくても core の PATH / Homebrew fallback が効くので失敗しない。
fn build_resolver() -> FallbackBinaryResolver {
    let mut resolver = FallbackBinaryResolver::new();

    // 実行ファイル隣接のバイナリを候補にする。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            resolver = resolver.with_resource_dir(dir.to_path_buf());
        }
    }

    resolver
}

/// `--output-dir` 未指定時の既定保存先 `<HOME>/Downloads` を返す。
///
/// `HOME` が取得できない (未設定 / 非UTF-8 / 空) 場合は明確なエラーを返す。
/// 既定パスは呼び出し側で存在しなければ作成する (明示指定は要存在のまま)。
fn default_downloads_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| {
        "保存先 (--output-dir) が未指定で、既定の ~/Downloads も解決できません \
     (環境変数 HOME が未設定)。--output-dir を明示してください。"
            .to_string()
    })?;
    if home.trim().is_empty() {
        return Err(
            "保存先 (--output-dir) が未指定で、既定の ~/Downloads も解決できません \
       (環境変数 HOME が空)。--output-dir を明示してください。"
                .to_string(),
        );
    }
    Ok(PathBuf::from(home).join("Downloads"))
}

fn run_url_to_mp3(args: UrlToMp3Args) -> Result<CliSummary, String> {
    if args.url.is_none() && args.input.is_none() {
        return Err("--url または --input のいずれかを指定してください。".to_string());
    }

    let resolver = build_resolver();
    let progress = make_progress_logger();

    // output_dir は任意。未指定時は ~/Downloads を既定にする。
    //   - 既定パス (~/Downloads): 無ければ作成する。
    //   - 明示指定パス: タイポで勝手に作らないよう「要存在」を維持する。
    let (output_dir, output_dir_is_default) = match args.output_dir.clone() {
        Some(dir) => (dir, false),
        None => (default_downloads_dir()?, true),
    };
    if output_dir_is_default {
        std::fs::create_dir_all(&output_dir).map_err(|error| {
            format!(
                "既定の保存先フォルダを作成できませんでした ({}): {error}",
                output_dir.display()
            )
        })?;
    } else if !output_dir.exists() {
        // 出力先の存在確認は core 側でも行うが、早期に分かりやすく出す。
        return Err(format!(
            "保存先フォルダが見つかりません: {}",
            output_dir.display()
        ));
    }

    // URL の場合: core の振り分け (route_for_url) に従う。
    //   - Direct  (直リンク / HLS): 従来どおり download_media -> 一時ファイル
    //     -> convert_media_blocking。
    //   - Extractor (Loom 等の埋め込み): yt-dlp で MP3 を直接生成。
    // ローカルファイルの場合: 従来どおり convert_media_blocking。
    if let Some(url) = args.url.as_ref() {
        let parsed =
            Url::parse(url).map_err(|error| format!("URL を解析できませんでした: {error}"))?;

        match route_for_url(&parsed) {
            MediaRoute::Direct => {
                // Speed-2/Speed-4 (GUI/MCP と同じ経路に統一): 「全 DL → 一時ファイル →
                // 逐次変換」をやめ、ffmpeg に URL (直リンク / HLS いずれも) を直接入力して
                // ストリーム変換する。DL とエンコードが重なり一時ファイルが不要になる。
                elog("直リンク / HLS をストリーム変換します (cookie/referer は redacted)。");
                let summary = convert_url_stream_to_mp3_with_cancel(
                    &resolver,
                    url,
                    &output_dir,
                    args.split_minutes,
                    args.optimize,
                    args.cookie.as_deref(),
                    args.referer.as_deref(),
                    None, // suggested_title: CLI には sniff 由来のタイトル候補が無い
                    &progress,
                    None, // cancel: CLI 実行は同期完走のみ (キャンセル機構なし)
                )?;
                Ok(CliSummary {
                    ok: true,
                    source_kind: "url-direct",
                    summary,
                })
            }
            MediaRoute::Extractor => {
                elog("埋め込み動画を yt-dlp で解決します (cookie/referer は redacted)。");
                let summary =
                    extract_via_ytdlp_to_summary(&resolver, url, &args, &output_dir, &progress)?;
                Ok(CliSummary {
                    ok: true,
                    source_kind: "url-ytdlp",
                    summary,
                })
            }
        }
    } else {
        let input = args.input.clone().expect("input is Some by guard above");
        if !input.exists() {
            return Err(format!("入力ファイルが見つかりません: {}", input.display()));
        }
        let summary = convert_local_to_mp3(
            &resolver,
            &input,
            &output_dir,
            args.split_minutes,
            args.optimize,
            &progress,
        )?;
        Ok(CliSummary {
            ok: true,
            source_kind: "local",
            summary,
        })
    }
}

/// ローカルファイル -> MP3 変換 (core が single source of truth)。
fn convert_local_to_mp3(
    resolver: &FallbackBinaryResolver,
    input: &Path,
    output_dir: &Path,
    split_minutes: Option<u64>,
    optimize: bool,
    progress: &(impl Fn(&Progress) -> Result<(), String> + Send + Sync),
) -> Result<ConversionSummary, String> {
    let request = ConversionRequest {
        input_path: input.to_string_lossy().to_string(),
        output_directory: output_dir.to_string_lossy().to_string(),
        split_minutes,
        optimize_for_size: optimize,
    };
    convert_media_blocking(resolver, request, progress)
}

/// yt-dlp で埋め込み動画を MP3 化し、`ConversionSummary` に詰め替える。
///
/// A8: 従来は CLI だけ「yt-dlp で生成 → 分割時 `convert_media_blocking`
/// (libmp3lame 再エンコード)」という MCP / host と異なる二重エンコード経路だった。
/// core の [`extract_url_to_mp3`] に一本化し、分割は core 内部で `segment_mp3_copy`
/// (`-c copy`) を使う形へ揃える。これにより A4 (出力レイアウトの job folder 統一) も
/// 同時に満たす (出力 dir 直下に生 MP3 を散らかさない)。
fn extract_via_ytdlp_to_summary(
    resolver: &FallbackBinaryResolver,
    url: &str,
    args: &UrlToMp3Args,
    output_dir: &Path,
    progress: &(impl Fn(&Progress) -> Result<(), String> + Send + Sync),
) -> Result<ConversionSummary, String> {
    extract_url_to_mp3(
        resolver,
        url,
        output_dir,
        args.split_minutes,
        args.optimize,
        args.cookie.as_deref(),
        args.referer.as_deref(),
        progress,
    )
    // YtDlpError -> String (ユーザ向け文言)。認証起因の種別判定は GUI / MCP 側が
    // 行う。CLI では文言をそのまま出す (秘密は含まれない)。
    .map_err(|error| error.to_string())
}
