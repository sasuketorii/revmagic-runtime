//! revmagic-core
//!
//! host 非依存の変換 / ダウンロード / probe コア。
//!
//! このクレートは `desktop-host` に依存しない。desktop host・CLI・MCP サーバが同一の
//! 変換ロジックを共有できるようにするための single source of truth。
//!
//! host 結合点の抽象化:
//! - 進捗: `Channel<ProgressPayload>` の代わりにコールバック
//!   `&(dyn Fn(&Progress) -> Result<(), String> + Send + Sync)` を受け取る。
//! - バイナリ解決: `AppHandle` の代わりに [`BinaryResolver`] を受け取る。
//!   `AppHandle` 無しでも PATH / Homebrew fallback で解決できる
//!   [`fallback_resolve_binary`] を提供する。
//! - cookie / referer: WebView 専属の cookie 取得は core に置かず、
//!   呼び出し側が解決済みの値を引数 (`Option<String>`) で渡す。

use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use chrono::Local;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use url::Url;

pub mod ytdlp;

pub use ytdlp::{
    build_ytdlp_args, cookie_header_to_netscape, extract_audio_via_ytdlp,
    extract_audio_via_ytdlp_with_deadline, extract_url_to_mp3, extract_url_to_mp3_with_cancel,
    is_direct_media, redact_url_queries, route_for_url, MediaRoute, YtDlpError, YtDlpErrorKind,
    YtDlpOptions,
};

const ALLOWED_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "mp3", "webm", "m4a"];

/// A14: 分割粒度 (`split_minutes`) の許容上限 (分)。10080 分 = 7 日。これを超える値は
/// 実用上意味が無く、`* 60` のオーバーフロー / 異常な segment_time を避けるため弾く。
pub const MAX_SPLIT_MINUTES: u64 = 10080;

/// A14: `split_minutes` を検証して秒へ変換する。
///
/// `1..=MAX_SPLIT_MINUTES` の範囲を要求し、範囲外はユーザ向けエラーを返す。範囲内の
/// 値は `* 60` で u64 オーバーフローしない (最大 604800 秒) が、防御的に
/// `saturating_mul` を使う。0 / 過大な分割指定での panic (debug) / wrap (release) /
/// 異常 segment_time を未然に防ぐ。
pub fn split_minutes_to_seconds(split_minutes: u64) -> Result<u64, String> {
    if split_minutes == 0 {
        return Err("分割する場合、分数は1以上にしてください。".into());
    }
    if split_minutes > MAX_SPLIT_MINUTES {
        return Err(format!(
            "分割の分数が大きすぎます (最大 {MAX_SPLIT_MINUTES} 分まで)。"
        ));
    }
    Ok(split_minutes.saturating_mul(60))
}

/// 直リンク DL の接続確立タイムアウト。TCP/TLS ハンドシェイクがこの時間内に
/// 完了しなければ接続失敗として打ち切る (無応答ホストでの無限ブロック解消)。
const DIRECT_DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 直リンク DL の read (idle) タイムアウト。
///
/// ## なぜ「全体」ではなく read(idle) なのか
/// 全体タイムアウト (`.timeout()`) は長尺・大容量の正常 DL を「健全に進行中
/// でも」打ち切ってしまうため採用しない。代わりに read タイムアウトを使い、
/// 「バイトが届かない無音状態」がこの時間続いた場合のみ打ち切る。これにより
/// 数十分かかる大容量 DL でも、進行している限り中断されない一方、サーバ無応答
/// / ストール時の無限ブロックは確実に断つ。
///
/// 値の根拠: 120 秒。一時的なネットワーク揺らぎ (数十秒級) では誤発火せず、
/// 2 分以上 1 バイトも届かない状態は実質ストールとみなす保守的な値。将来
/// MCP / CLI 引数で上書きする余地を残すため定数化している (本スライスでは内部
/// デフォルトのみ・引数契約は不変)。
const DIRECT_DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// 直リンク DL の await を cancel トークンと race させるときのポーリング間隔
/// (Slice I-1c 指摘1)。yt-dlp / ffmpeg 経路の 100ms 粒度と揃え、無音接続
/// (`send()` 接続待ち / body 受信の無音) でも ~100ms 以内でキャンセルへ反応させる。
const DIRECT_DOWNLOAD_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// フロントからの変換リクエスト。host side invoke 引数と同一の JSON 形状。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionRequest {
    pub input_path: String,
    pub output_directory: String,
    pub split_minutes: Option<u64>,
    pub optimize_for_size: bool,
}

/// 出力ファイル 1 件のサマリ。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSummary {
    pub path: String,
    pub size_bytes: u64,
}

/// 変換結果サマリ。host side戻り値と同一の JSON 形状。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionSummary {
    pub output_folder: String,
    pub file_count: usize,
    pub total_size_bytes: u64,
    pub files: Vec<FileSummary>,
    pub duration_seconds: f64,
}

/// 進捗ペイロード。host side `ProgressPayload` と同一の JSON 形状
/// (Channel 経由でフロントへ送信される)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub percent: u8,
    pub step: String,
    pub detail: String,
    pub current_seconds: f64,
    pub total_seconds: f64,
}

/// 進捗コールバック型。
///
/// 呼び出し側 (desktop host) はこのコールバック内で
/// `channel.send(ProgressPayload { .. })` を行うブリッジを提供する。
///
/// 旧 host 実装の `emit_progress(...)?` 挙動を保つため、送信に失敗した場合は
/// `Err(String)` を返す。core 側の全 progress 呼び出しは `?` で伝播させ、
/// 進捗送信が失敗した時点で変換 / ダウンロード処理を中断する。
pub type ProgressFn<'a> = &'a (dyn Fn(&Progress) -> Result<(), String> + Send + Sync);

/// 協調的キャンセルトークン (Slice I-1 reviewer fix / 指摘2)。
///
/// `Some(flag)` を渡すと、core の長時間ブロックし得るループ
/// (yt-dlp 進捗ポーリング / 直リンク DL chunk ループ / ffmpeg 行読みループ) が
/// **各反復でフラグを確認** し、立っていれば child プロセス (グループ) を kill して
/// 即座に中断エラーを返す。これにより、進捗通知が来ない無音ハング時でも
/// (yt-dlp 無音 = 最大 deadline、直 DL = read timeout 待ち) を待たず ~100ms 以内で
/// 停止できる。
///
/// `None` (既定) なら従来挙動と完全に同一 (キャンセルチェックを一切行わない)。
/// MCP / CLI 既存呼び出しは `None` 経路の公開 API を使うため挙動不変。
///
/// core は host 非依存を維持するため `std::sync::atomic::AtomicBool` のみに依存し、
/// `desktop-host` 型は使わない。GUI 側 (`CancelFlag` の `Arc<AtomicBool>`) は `Some(&flag)`
/// で渡す。
pub type Cancel<'a> = Option<&'a std::sync::atomic::AtomicBool>;

/// キャンセルトークン経路で中断した時の固定ユーザ向け文言。
///
/// progress コールバック経路 (`cancellable_channel_progress`) が返す文言
/// (`"キャンセルされました"`) と揃え、GUI のキャンセル判定 (`cancel_flag` を見て
/// `ConvertError::cancelled()` へ写像) と一貫させる。secret は含まない。
pub(crate) const CANCEL_MESSAGE: &str = "キャンセルされました";

/// A18: 直リンク / HLS 経路で認証起因 (HTTP 401 / 403) の失敗を、文字列エラーの
/// 先頭に付ける機械可読マーカー。
///
/// ## 背景と設計
/// `download_media` / `download_hls_blocking` 系は後方互換のため `Result<_, String>`
/// を返す (MCP / host が `String` として消費)。yt-dlp 経路の [`YtDlpErrorKind`] の
/// ような型付きエラーへ全面移行すると既存呼び出し側のコンパイルが壊れるため、
/// **戻り値型は変えず**、認証失敗のときだけエラー文言の **先頭にこのマーカーを付与**
/// する。上位 (host の `convert_url` 等) は [`is_auth_required_error`] でこれを判定し
/// `authRequired` 種別へ写像できる。yt-dlp 側の `AuthRequired` 分類と意味的に整合する。
///
/// マーカーは ASCII 固定文字列で、秘密情報を含まない。表示用文言は
/// [`strip_auth_required_marker`] で除去できる。
pub const AUTH_REQUIRED_MARKER: &str = "[AUTH_REQUIRED] ";

/// A18: エラー文言が [`AUTH_REQUIRED_MARKER`] で始まる (= 認証が必要) かどうか。
///
/// 直リンク / HLS 経路の `String` エラーを上位が型判定するための公開ヘルパ。
/// yt-dlp 経路の [`YtDlpError::is_auth_error`] に対応する直リンク経路版。
pub fn is_auth_required_error(message: &str) -> bool {
    message.starts_with(AUTH_REQUIRED_MARKER)
}

/// A18: 表示用に [`AUTH_REQUIRED_MARKER`] を取り除いたユーザ向け文言を返す。
///
/// マーカーが無ければ入力をそのまま返す。
pub fn strip_auth_required_marker(message: &str) -> &str {
    message
        .strip_prefix(AUTH_REQUIRED_MARKER)
        .unwrap_or(message)
}

/// ffmpeg / ffprobe などのバイナリ解決を抽象化する。
///
/// desktop hostは `AppHandle` の `resource_dir` を優先する実装を提供し、
/// AppHandle が無い文脈 (CLI / MCP / テスト) では
/// [`FallbackBinaryResolver`] が PATH / Homebrew fallback で解決する。
pub trait BinaryResolver {
    /// `binary` (例: `"ffmpeg"`, `"ffprobe"`) の絶対パスを解決する。
    fn resolve(&self, binary: &str) -> Result<PathBuf, String>;
}

/// AppHandle 非依存の既定リゾルバ。
///
/// 探索順:
/// 1. `local_dirs` 配下の `binaries/<binary>` (実行ファイル隣接の working slot)
/// 2. `resource_dirs` 配下の `binaries/<binary>` (配布 bundle の self-contained slot)
/// 3. Homebrew / 標準パス (`/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`)
/// 4. `which <binary>` の結果
///
/// 全ディレクトリを空にすれば純粋な PATH / Homebrew fallback になる。
///
/// ## bundled (self-contained) と local (working) の区別 (F1 指摘1)
/// `resource_dirs` は **配布 bundle の self-contained slot** (例: host の
/// `Resources/`)。ここに置かれる yt-dlp は配布先で単体動作する前提なので、配布先で
/// 壊れる極小 text-script shim / 0-byte placeholder は採用しない (skip して次候補へ)。
///
/// `local_dirs` は **実行ファイル隣接の working slot** (例: `current_exe()` の親)。
/// ここに置かれる shim は「いま・ここで動く」ローカル候補なので、極小 shebang でも
/// (正常な Homebrew entry-point shim 等と同様に) 受理する。
#[derive(Debug, Clone, Default)]
pub struct FallbackBinaryResolver {
    /// working slot (shim 受理)。bundled より前に探索する。
    local_dirs: Vec<PathBuf>,
    /// self-contained bundle slot (shim 拒否)。
    resource_dirs: Vec<PathBuf>,
}

impl FallbackBinaryResolver {
    /// PATH / Homebrew fallback のみで解決するリゾルバを作る。
    pub fn new() -> Self {
        Self {
            local_dirs: Vec::new(),
            resource_dirs: Vec::new(),
        }
    }

    /// 配布 bundle (self-contained) の `binaries/<binary>` を探索する追加ディレクトリを
    /// 登録する。極小 text-script shim / placeholder は採用しない (shim 拒否)。
    ///
    /// desktop hostは `resource_dir` / `executable_dir/../Resources` を
    /// ここに積んで resource 優先の挙動を再現する。
    pub fn with_resource_dir(mut self, dir: PathBuf) -> Self {
        self.resource_dirs.push(dir);
        self
    }

    /// 実行ファイル隣接など **working slot** の `binaries/<binary>` を探索する追加
    /// ディレクトリを登録する。ここの候補は「その場で動く」前提なので極小 shebang shim
    /// でも受理する (PATH 候補と同等扱い)。
    pub fn with_local_dir(mut self, dir: PathBuf) -> Self {
        self.local_dirs.push(dir);
        self
    }
}

impl BinaryResolver for FallbackBinaryResolver {
    fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
        fallback_resolve_binary_with_local(binary, &self.local_dirs, &self.resource_dirs)
    }
}

/// 与えられた resource ディレクトリ群を優先し、PATH / Homebrew fallback で
/// `binary` の絶対パスを解決する。
///
/// `resource_dirs` の各要素について `<dir>/binaries/<binary>` を候補に積む。
pub fn fallback_resolve_binary(binary: &str, resource_dirs: &[PathBuf]) -> Result<PathBuf, String> {
    fallback_resolve_binary_with_local(binary, &[], resource_dirs)
}

/// `local_dirs` (working slot) を最優先したのち `resource_dirs` (self-contained
/// bundle slot)、system path、`which` の順で `binary` を解決する。
///
/// `local_dirs` 配下の `<dir>/binaries/<bin>` は working 扱い (shim 受理)、
/// `resource_dirs` 配下の `<dir>/binaries/<bin>` は bundled 扱い (shim 拒否) になる
/// (F1 指摘1)。
pub fn fallback_resolve_binary_with_local(
    binary: &str,
    local_dirs: &[PathBuf],
    resource_dirs: &[PathBuf],
) -> Result<PathBuf, String> {
    // 実 system path (Homebrew / 標準パス) を bin ディレクトリとして渡す。
    let system_dirs = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ];

    // A3: `which` はサブプロセスを spawn するため、変換 1 回で ffmpeg/ffprobe/yt-dlp を
    // 何度も解決すると無視できないオーバーヘッド (「解決中」の体感遅延の一因) になる。
    // 結果を **プロセス内キャッシュ** して同一 binary の `which` 再実行を避ける。
    // `which` は通常 PATH 内の絶対パスを返し、プロセス生存中に変わらない前提なので
    // キャッシュは安全。local/resource/system 候補が先に採用されればここには到達しない。
    let which_candidate = cached_which(binary);

    resolve_binary_inner(
        binary,
        local_dirs,
        resource_dirs,
        &system_dirs,
        which_candidate,
    )
}

/// A3: `which <binary>` の結果をプロセス内でキャッシュして返す。
///
/// 初回のみ `which` を spawn し、結果 (`Some(path)` / `None`) を保持する。以降は
/// キャッシュを返し、サブプロセス起動コストを省く。`Once* + Mutex<HashMap>` で
/// binary ごとに 1 度だけ解決する。
fn cached_which(binary: &str) -> Option<PathBuf> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    // poison しても中身は単なるキャッシュなので into_inner で回復する。
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = guard.get(binary) {
        return cached.clone();
    }

    let resolved = Command::new("which")
        .arg(binary)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|resolved| !resolved.is_empty())
        .map(PathBuf::from);

    guard.insert(binary.to_string(), resolved.clone());
    resolved
}

/// `fallback_resolve_binary` の本体 (system path ディレクトリと `which` 候補を
/// 引数化したテスト可能な seam)。
///
/// 候補順: local (`<local_dir>/binaries/<bin>`, working) -> bundled
/// (`<resource_dir>/binaries/<bin>`, self-contained) -> system path
/// (`<system_dir>/<bin>`) -> `which` 結果。出所 (origin) で yt-dlp の shim 採否を
/// 切り替える (指摘1)。`#[doc(hidden)]` は内部 API のため。
#[doc(hidden)]
pub fn resolve_binary_inner(
    binary: &str,
    local_dirs: &[PathBuf],
    resource_dirs: &[PathBuf],
    system_dirs: &[PathBuf],
    which_candidate: Option<PathBuf>,
) -> Result<PathBuf, String> {
    // 候補の出所 (origin) を保持する。bundled (self-contained) 候補だけ yt-dlp の shim を
    // 拒否し、local / system path / which 候補は working 扱いで受理する (指摘1)。
    let mut candidates: Vec<(PathBuf, CandidateOrigin)> = Vec::new();

    // working slot を最優先 (shim 受理)。
    for dir in local_dirs {
        candidates.push((dir.join("binaries").join(binary), CandidateOrigin::Working));
    }

    // self-contained bundle slot (shim 拒否)。
    for dir in resource_dirs {
        candidates.push((dir.join("binaries").join(binary), CandidateOrigin::Bundled));
    }

    for dir in system_dirs {
        candidates.push((dir.join(binary), CandidateOrigin::Working));
    }

    if let Some(resolved) = which_candidate {
        candidates.push((resolved, CandidateOrigin::Working));
    }

    candidates
    .into_iter()
    .find(|(candidate, origin)| is_usable_binary_for(binary, candidate, *origin))
    .map(|(candidate, _)| candidate)
    .ok_or_else(|| {
      format!(
        "{binary} が見つかりません。アプリ同梱に失敗しているか、ビルド環境に {binary} がありません。"
      )
    })
}

/// 候補バイナリの出所。yt-dlp の shim 拒否を bundled (self-contained) 候補のみに
/// 限定するために使う (指摘1)。
///
/// - [`CandidateOrigin::Bundled`]: 配布 bundle の self-contained slot
///   (`<resource_dir>/binaries/<bin>`)。配布先で単体動作する前提なので、配布先で壊れる
///   極小 text-script shim / placeholder は採用しない (次候補へ fallthrough)。
/// - [`CandidateOrigin::Working`]: 実行ファイル隣接 (local slot) / PATH / Homebrew /
///   `which` 上の候補。ユーザー環境で **その場で動く** ものなので、極小 shebang でも
///   (正常な Homebrew entry-point 等) 従来どおり受理する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateOrigin {
    Bundled,
    Working,
}

/// yt-dlp の同梱実体として不適格と判断する text-script (shim/wrapper) の上限
/// サイズ (byte)。これ未満かつ shebang (`#!`) で始まる候補は「self-contained な
/// 実 yt-dlp ではなく launcher shim / placeholder」とみなし、同梱実体としては
/// 採用しない。
///
/// 配布先で壊れる代表例は 195 byte の Homebrew Python launcher shim
/// (`#!/opt/homebrew/Cellar/.../python` を絶対パス参照) で、配布先には
/// その python が存在せず壊れる。一方、公式 yt-dlp standalone (arm64) は
/// 数 MB の self-contained binary なのでこの閾値を大きく上回り、誤って弾かない。
const YTDLP_SHIM_MAX_BYTES: u64 = 64 * 1024;

/// 候補ファイルが shebang (`#!`) で始まる text-script か判定する。
///
/// 先頭 2 byte だけ読む。読めない場合は「script ではない」と保守的に倒す
/// (別の usable 判定で弾けるなら弾く)。
fn starts_with_shebang(candidate: &Path) -> bool {
    use std::io::Read as _;
    let mut file = match fs::File::open(candidate) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut head = [0u8; 2];
    match file.read(&mut head) {
        Ok(2) => &head == b"#!",
        _ => false,
    }
}

/// 候補パスが「採用可能な実バイナリ」か判定する (binary 非依存の共通条件)。
///
/// `build.rs` は yt-dlp 不在時に `binaries/yt-dlp` へ **0 byte / 0755 の
/// placeholder sentinel** を置く。これを `exists()` だけで掴むと、PATH や
/// Homebrew に実 yt-dlp があっても空ファイルを実行して必ず失敗する。そこで
/// 採用条件に以下を課す:
/// - 通常ファイル (regular file) であること。
/// - **サイズが 0 byte でない** こと (placeholder を skip)。
/// - unix では実行可能 bit が立っていること (環境差を踏まないよう unix 限定)。
///
/// 0 byte / 実行不可の候補は skip し、resolver は次候補 (別 resource dir ->
/// Homebrew/標準パス -> which) へ fallthrough する。同梱の実 ffmpeg / ffprobe は
/// 非 0 byte / 0755 なのでこの判定を通り、解決は回帰しない。
fn is_usable_binary(candidate: &Path) -> bool {
    let metadata = match fs::metadata(candidate) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if !metadata.is_file() {
        return false;
    }
    // placeholder sentinel (0 byte) を弾く。
    if metadata.len() == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // いずれかの実行可能 bit (owner/group/other) が立っていること。
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

/// `binary` 向けに候補が採用可能か判定する (F1: resolver 安全化)。
///
/// 共通条件 ([`is_usable_binary`]) に加え、`yt-dlp` の **bundled (同梱) 候補** に
/// 限り **極小サイズの text-script shim** (shebang で始まり [`YTDLP_SHIM_MAX_BYTES`]
/// 未満) を「同梱実体としては不適格」とみなし採用しない。bundled は self-contained
/// 前提であり、Homebrew Python launcher shim を同梱すると配布先で参照先 python が
/// 無く壊れるためである。この場合 resolver は次候補 (PATH / Homebrew / which 上の
/// 動作する yt-dlp) へ fallthrough する。
///
/// 一方 **PATH / Homebrew / `which` 候補** ([`CandidateOrigin::Working`]) は
/// shim 判定を課さない。ユーザー環境で **その場で動く** ものなので、195 byte の
/// Homebrew entry-point shim (`--version` が正常応答する) を弾いてはいけない
/// (指摘1: 回帰是正)。共通条件 ([`is_usable_binary`]: regular file / 非0byte /
/// 実行可能) のみで採否する。
///
/// `ffmpeg` / `ffprobe` には追加判定を課さない (既存解決を回帰させない)。
fn is_usable_binary_for(binary: &str, candidate: &Path, origin: CandidateOrigin) -> bool {
    if !is_usable_binary(candidate) {
        return false;
    }
    // shim 拒否は yt-dlp の bundled 候補にだけ適用する。PATH/Homebrew/which 候補は
    // その場で動く前提なので極小 shebang でも受理する。
    if binary == "yt-dlp" && origin == CandidateOrigin::Bundled {
        // self-contained な実 yt-dlp は数 MB。極小 shebang script は launcher
        // shim / placeholder とみなし、同梱実体としては採用しない。
        let len = fs::metadata(candidate).map(|m| m.len()).unwrap_or(0);
        if len < YTDLP_SHIM_MAX_BYTES && starts_with_shebang(candidate) {
            return false;
        }
    }
    true
}

/// URL が HLS (m3u8) を指すか判定する。
pub fn is_hls_url(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.contains(".m3u8/")
}

fn build_ffmpeg_http_headers(cookie: Option<&str>, referer: Option<&str>) -> String {
    let mut headers = String::new();
    headers.push_str(
        "User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15\r\n",
    );
    if let Some(value) = referer.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push_str(&format!("Referer: {}\r\n", value));
    }
    if let Some(value) = cookie.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push_str(&format!("Cookie: {}\r\n", value));
    }
    headers
}

/// HLS の duration 解析用 ffprobe を起動するときのデッドライン。
///
/// 無音 HLS (m3u8) に対し ffprobe がプレイリスト / セグメントの取得待ちで
/// 永久にハングし得るため、上限を設けてプロセスグループごと kill する。HLS の
/// 結合 ffmpeg 段 ([`run_ffmpeg_with_cancel`]) と同じ規律を duration 解析段にも
/// 適用するための保守的上限。長さ取得は通常数秒で終わるが、ネットワーク無応答を
/// 放置しないために 60 秒で打ち切る。
const HLS_FFPROBE_DURATION_DEADLINE: Duration = Duration::from_secs(60);

/// ffprobe 進捗 (stdout) 監視 / cancel トークン確認のポーリング間隔。
/// yt-dlp / ffmpeg 経路と同じ 100ms 粒度で cancel に反応できるようにする。
const FFPROBE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// URL から ffprobe で duration (秒) を取得する協調的キャンセル + タイムアウト
/// 対応版 (Slice I-1c reviewer fix / 指摘1)。
///
/// ffprobe を **新プロセスグループのリーダー** (Unix) として spawn し、stdout を
/// 別スレッドで吸い出しつつ、main スレッドは [`FFPROBE_POLL_INTERVAL`] (100ms)
/// 粒度でポーリングして以下を確認する:
/// - `cancel` トークンが立ったら [`ytdlp::kill_and_reap`] (killpg + bounded reap)
///   でグループ (= ffprobe 子孫含む) を kill し、`CANCEL_MESSAGE` で中断する。
/// - `deadline` を `Some` で渡し超過したら同様に killpg し、長さ取得失敗エラーを
///   返す。
///
/// これにより無音 HLS で ffprobe がハングしても ~100ms 以内 (cancel) /
/// deadline 以内 (timeout) で確実に中断でき、HLS 結合 ffmpeg 前段の cancel 不能
/// 区間が無くなる。
///
/// 後方互換: `cancel = None` かつ `deadline = None` なら、ffprobe 完了まで待つ
/// 従来の同期挙動と同一になる (非 HLS 経路の回帰なし)。
fn read_duration_seconds_for_url_with_cancel(
    ffprobe: &Path,
    url: &str,
    headers: &str,
    deadline: Option<Duration>,
    cancel: Cancel<'_>,
) -> Result<f64, String> {
    let started = Instant::now();
    let mut command = Command::new(ffprobe);
    command
        .args(["-v", "error", "-headers"])
        .arg(headers)
        .args([
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            url,
        ])
        // SIGTTIN 根絶: process_group(0) で独立グループ化した ffprobe が端末 stdin を
        // 読もうとして停止しないよう、stdin を /dev/null に差し替える。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 指摘1: ffprobe を新プロセスグループのリーダーにする (Unix)。ffprobe は通常
    // 子孫を spawn しないが、HLS でプロトコルハンドラ等が子孫を握るケースに備え、
    // ffmpeg / yt-dlp と同じく killpg でグループ一括 kill を効かせる。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("ffprobe の実行に失敗しました: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffprobe の出力を取得できませんでした。".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "ffprobe のエラー出力を取得できませんでした。".to_string())?;

    // stdout / stderr を別スレッドで全量読みする。これにより ffprobe が無音の
    // まま生き続けても main スレッドが read で無限ブロックせず、poll で cancel /
    // deadline を監視できる (yt-dlp / ffmpeg と同じ構造)。
    let stdout_handle = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;
        Ok(buffer)
    });
    let stderr_handle = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut reader = BufReader::new(stderr);
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;
        Ok(buffer)
    });

    // poll ループ: cancel / deadline / プロセス終了を 100ms 粒度で確認する。
    let status = loop {
        // 指摘1: cancel トークンが立っていれば killpg + bounded reap して即中断。
        if ytdlp::is_cancelled(cancel) {
            ytdlp::kill_and_reap(&mut child);
            bounded_join_ffmpeg_stderr(stdout_handle);
            bounded_join_ffmpeg_stderr(stderr_handle);
            return Err(CANCEL_MESSAGE.to_string());
        }
        // deadline 超過時も killpg してタイムアウトエラー化。
        if let Some(limit) = deadline {
            if started.elapsed() >= limit {
                ytdlp::kill_and_reap(&mut child);
                bounded_join_ffmpeg_stderr(stdout_handle);
                bounded_join_ffmpeg_stderr(stderr_handle);
                return Err("メディアの長さを取得できませんでした (制限時間超過)。".to_string());
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(FFPROBE_POLL_INTERVAL),
            Err(error) => {
                ytdlp::kill_and_reap(&mut child);
                bounded_join_ffmpeg_stderr(stdout_handle);
                bounded_join_ffmpeg_stderr(stderr_handle);
                return Err(format!("ffprobe の終了待機に失敗しました: {error}"));
            }
        }
    };

    let stdout_bytes = stdout_handle
        .join()
        .map_err(|_| "ffprobe の出力スレッドが異常終了しました。".to_string())?
        .map_err(|error| format!("ffprobe の出力を読み取れませんでした: {error}"))?;
    let stderr_bytes = stderr_handle
        .join()
        .map_err(|_| "ffprobe のエラー出力スレッドが異常終了しました。".to_string())?
        .map_err(|error| format!("ffprobe のエラー出力を読み取れませんでした: {error}"))?;

    if !status.success() {
        return Err(stderr_message(
            "メディアの長さを取得できませんでした。",
            &stderr_bytes,
        ));
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    stdout
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("メディアの長さを解析できませんでした: {error}"))
}

/// HLS (m3u8) を ffmpeg でダウンロードして一時 mp4 を作り、そのパスを返す。
///
/// `cookie` / `referer` は呼び出し側が解決済みの値を渡す。`cookie` が空 /
/// 未指定でも core 側では login window 由来の cookie 取得は行わない
/// (WebView 専属ロジックは host sideに残す)。
pub fn download_hls_blocking<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: Url,
    cookie: Option<String>,
    referer: Option<String>,
    progress: ProgressFn<'_>,
) -> Result<String, String> {
    // 後方互換: cancel トークン無し (None)。MCP / CLI 既存呼び出しは挙動不変。
    download_hls_blocking_with_cancel(resolver, url, cookie, referer, progress, None)
}

/// [`download_hls_blocking`] の協調的キャンセル対応版 (Slice I-1c 指摘2)。
///
/// `cancel` に `Some(flag)` を渡すと、HLS 結合の ffmpeg 実行を
/// [`run_ffmpeg_with_cancel`] で行う。これにより ffmpeg 行読みループの各反復で
/// トークンが確認され、無音 ffmpeg (進捗を出さない HLS セグメント取得中) でも
/// ~100ms 以内で ffmpeg child (グループ = 子孫含む) を killpg して中断する。
/// `None` なら従来挙動と同一 (cancel チェックを一切行わない)。
///
/// 指摘1 (Slice I-1c reviewer fix): duration 解析の ffprobe も
/// [`read_duration_seconds_for_url_with_cancel`] 経由で **cancel トークン +
/// 上限 deadline ([`HLS_FFPROBE_DURATION_DEADLINE`])** 付きで実行する。無音 HLS
/// で ffprobe がハングしても cancel なら ~100ms、無 cancel でも deadline で
/// killpg され、結合 ffmpeg 段に入る前の cancel 不能区間を無くす。cancel 時は
/// 中断エラーを伝播し、duration 取得 timeout / 解析失敗時は長さ不明 (0.0) として
/// 変換を続行する。
pub fn download_hls_blocking_with_cancel<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: Url,
    cookie: Option<String>,
    referer: Option<String>,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
) -> Result<String, String> {
    let ffmpeg = resolver.resolve("ffmpeg")?;
    let ffprobe = resolver.resolve("ffprobe")?;

    progress(&Progress {
        percent: 0,
        step: "ダウンロード準備中".into(),
        detail: "HLS プレイリストを解析しています。".into(),
        current_seconds: 0.0,
        total_seconds: 0.0,
    })?;

    let cookie_to_use = cookie
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let default_referer = match url.host_str() {
        Some(host) => format!("{}://{}/", url.scheme(), host),
        None => format!("{}://", url.scheme()),
    };
    let referer_to_use = referer
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or(default_referer);

    let headers =
        build_ffmpeg_http_headers(cookie_to_use.as_deref(), Some(referer_to_use.as_str()));

    let url_string = url.to_string();
    // 指摘1 (Slice I-1c): duration 解析 ffprobe を cancel トークン + 上限 deadline
    // 付きで実行する。cancel が立っていれば中断エラーを伝播 (~100ms で killpg)。
    // timeout / 解析失敗など cancel 以外の失敗は長さ不明 (0.0) として変換を続行する
    // (従来の `unwrap_or(0.0)` と同じ堅牢性)。
    let duration_seconds = match read_duration_seconds_for_url_with_cancel(
        &ffprobe,
        &url_string,
        &headers,
        Some(HLS_FFPROBE_DURATION_DEADLINE),
        cancel,
    ) {
        Ok(seconds) => seconds,
        Err(message) if message == CANCEL_MESSAGE => return Err(message),
        Err(_) => 0.0,
    };

    let stem = extract_url_stem(&url);
    let safe_stem = sanitize_name(&stem);
    let temp_base = std::env::temp_dir().join("revmagic-downloads");
    let timestamp = Local::now().format("%Y%m%d-%H%M%S-%f").to_string();
    // 共通ヘルパで `<ts>` サブディレクトリを排他作成する (create_dir_all は親
    // (temp_base) の用意にのみ使い、candidate 自体は fs::create_dir で排他作成)。
    // これにより以下でアームする TempDirGuard の対象が確実に新規・排他になる。
    let temp_dir = create_exclusive_dir(&temp_base, &timestamp)?;
    // A5/A6: temp_dir 作成直後に RAII ガードを張る。以降の `?` 早期 return
    // (ffmpeg 失敗 / 401/403 / cancel / progress エラー) の全経路で Drop が
    // temp_dir を掃除し、部分 .mp4 / 空フォルダを残さない。成功時のみ disarm する。
    let mut temp_guard = TempDirGuard::arm(temp_dir.clone());
    let temp_file = temp_dir.join(format!("{safe_stem}.mp4"));

    let mut args: Vec<OsString> = vec![
        OsString::from("-y"),
        // -nostdin: ffmpeg が stdin を対話的に読まないことを明示 (SIGTTIN ハング防止)。
        OsString::from("-nostdin"),
        OsString::from("-headers"),
        OsString::from(&headers),
        OsString::from("-i"),
        OsString::from(&url_string),
        OsString::from("-c"),
        OsString::from("copy"),
        OsString::from("-bsf:a"),
        OsString::from("aac_adtstoasc"),
        OsString::from("-movflags"),
        OsString::from("+faststart"),
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
    ];
    args.push(temp_file.as_os_str().into());

    // 指摘2 (Slice I-1c): cancel トークンを渡し、HLS 結合の ffmpeg を
    // token-cancellable にする (無音 ffmpeg でも ~100ms で killpg)。
    // A18: HLS の本体取得 (ffmpeg) が 401 / 403 で失敗した場合は認証起因として
    // マーカーを付与する。ffmpeg は HTTP ステータスを stderr 文言で出すため、文言を
    // 走査して 401/403 を検出する。cancel 文言はそのまま伝播 (マーカーを付けない)。
    run_ffmpeg_with_cancel(
        progress,
        &ffmpeg,
        args,
        duration_seconds,
        "ダウンロード中",
        "HLS セグメントを取得しています。",
        cancel,
    )
    .map_err(|message| {
        // cancel 文言は固定 ASCII (URL 非含有) なのでそのまま伝播。それ以外は ffmpeg
        // stderr 由来で入力 URL (`?token=...` 等の署名/期限クエリ込み) を含み得るため、
        // 必ず redact_url_queries を通してから返す。direct 経路 (~962/1049) と同じ規律。
        // AUTH_REQUIRED_MARKER は固定 ASCII の機械可読プレフィクスなので、redact 後に
        // 付与する (redact → marker 付与の順)。これにより is_auth_required_error /
        // strip_auth_required_marker の挙動は不変のまま、表示文言から URL 秘密を除く。
        if message == CANCEL_MESSAGE {
            message
        } else {
            let redacted = redact_url_queries(&message);
            if looks_like_http_auth_failure(&redacted) {
                format!("{AUTH_REQUIRED_MARKER}{redacted}")
            } else {
                redacted
            }
        }
    })?;

    progress(&Progress {
        percent: 100,
        step: "ダウンロード完了".into(),
        detail: "変換を開始します。".into(),
        current_seconds: duration_seconds,
        total_seconds: duration_seconds,
    })?;

    // 成功: temp_file は呼び出し側が変換後に cleanup_download_temp で消す。ここで
    // 消さないようガードを disarm する (disarm 漏れ = 成果物の誤削除に注意)。
    temp_guard.disarm();
    Ok(temp_file.to_string_lossy().to_string())
}

/// Speed-2/Speed-4: 直リンク URL・HLS (`.m3u8`) 双方を **ffmpeg に直接入力** して
/// MP3 へ変換する (DL と変換を重ね、一時 DL ファイルを不要にする)。
///
/// 従来の `convert_direct_url` (desktop-host) は「全 DL → 一時ファイル → 変換」と
/// 逐次だったため、507MB の一時書き出し + 逐次変換で体感が遅かった。本関数は
/// ffmpeg の `-headers` に Cookie / Referer を載せ、URL をそのまま `-i` 入力にして
/// mp3 へ再エンコードする。ffmpeg が HTTP 取得とエンコードを同時に行うため、DL と
/// 変換が重なり一時ファイルが不要になる。
///
/// ## HLS 対応 (Speed-4)
/// 以前は HLS (`.m3u8`) のみ [`download_hls_blocking_with_cancel`] で「DL(`-c copy`
/// 結合)→ 一時 mp4 → ローカル変換」の逐次2段構成を維持していたが、ffmpeg の HLS
/// demuxer は `-i <m3u8-url>` を直リンクと同じ入力として扱え、`-headers` は
/// プレイリスト取得だけでなくセグメント取得の HTTP リクエストにも適用される
/// (ffmpeg の既定挙動)。そのため本関数は HLS URL もそのまま受け取り、単一パスで
/// MP3 へ直接エンコードする。VOD (終端のある `#EXT-X-ENDLIST` 付き) HLS を前提とする
/// (ライブ配信の継続録音は元々サポート対象外)。
///
/// ## 分割併用時の方式 (コーダー判断・根拠)
/// 分割指定があるとき、リモート URL に対しセグメントごとの `-ss/-t` 個別 ffmpeg を
/// かけると **セグメント数ぶん再ダウンロード** が発生し、HTTP の Range 非対応 /
/// 署名 URL の期限切れ等で破綻し得る。そこで分割時は **一旦ストリームで単一の
/// 正規化済み mp3 を出力フォルダ内の作業ファイルへ書き出し**、その mp3 を
/// [`build_segment_copy_args`] (Speed-3: `-c copy` 単一 ffmpeg・再エンコードなし)
/// でローカル分割する。work_mp3 は既に `base_audio_args` で正規化済みのため、
/// 分割段で再エンコードする必要はない (以前は [`run_split_conversion`] で
/// libmp3lame の二重エンコードをしていたが、stream copy に置き換えて解消した;
/// yt-dlp 経路の [`segment_mp3_copy_with_cancel`] と同じ設計)。これにより
/// 「DL は 1 回・変換と重複」かつ「分割はエンコード不要の軽量コピー」という
/// 現実的で速い方式になる (従来の「全 DL → 一時 → 逐次変換」より速い)。
///
/// ## 認証 / cancel / 進捗
/// - 401/403 は ffmpeg stderr 文言から [`looks_like_http_auth_failure`] で検出し、
///   [`AUTH_REQUIRED_MARKER`] を付与する (直リンク / HLS 経路と整合)。
/// - cancel トークンは [`run_ffmpeg_with_cancel`] へ伝播し、無音 ffmpeg でも
///   ~100ms で killpg される。
/// - 進捗は ffmpeg の `-progress` をパースして発火する。
/// - エラー / cancel 時の部分成果物は [`TempDirGuard`] が掃除する。
#[allow(clippy::too_many_arguments)]
pub fn convert_url_stream_to_mp3_with_cancel<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: &str,
    output_directory: &Path,
    split_minutes: Option<u64>,
    optimize_for_size: bool,
    cookie: Option<&str>,
    referer: Option<&str>,
    suggested_title: Option<&str>,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
) -> Result<ConversionSummary, String> {
    let parsed = Url::parse(url).map_err(|error| format!("URL を解析できませんでした: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("http または https の URL を指定してください。".into());
    }
    if !output_directory.exists() {
        return Err("保存先フォルダが見つかりません。".into());
    }

    // split 範囲検証 (1..=MAX_SPLIT_MINUTES) を早期に弾く。
    if let Some(minutes) = split_minutes {
        let _ = split_minutes_to_seconds(minutes)?;
    }

    let ffmpeg = resolver.resolve("ffmpeg")?;
    let ffprobe = resolver.resolve("ffprobe")?;

    progress(&Progress {
        percent: 0,
        step: "準備中".into(),
        detail: "ストリーム変換を準備しています。".into(),
        current_seconds: 0.0,
        total_seconds: 0.0,
    })?;

    let cookie_to_use = cookie
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let default_referer = match parsed.host_str() {
        Some(host) => format!("{}://{}/", parsed.scheme(), host),
        None => format!("{}://", parsed.scheme()),
    };
    let referer_to_use = referer
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or(default_referer);
    let headers =
        build_ffmpeg_http_headers(cookie_to_use.as_deref(), Some(referer_to_use.as_str()));

    let url_string = parsed.to_string();
    // 長さ取得 (進捗の分母)。cancel + 上限 deadline 付き。timeout / 解析失敗は不明
    // (0.0) として続行する (HLS 経路と同じ堅牢性)。
    let duration_seconds = match read_duration_seconds_for_url_with_cancel(
        &ffprobe,
        &url_string,
        &headers,
        Some(HLS_FFPROBE_DURATION_DEADLINE),
        cancel,
    ) {
        Ok(seconds) => seconds,
        Err(message) if message == CANCEL_MESSAGE => return Err(message),
        Err(_) => 0.0,
    };

    // sniff 窓のページタイトルが取れていればそれを優先する (URL 末尾セグメントは
    // CDN のトークン/ID 名であることが多く、人が見て分からない)。空白のみ等は
    // 空文字扱いにして URL stem へフォールバックする。
    let stem = suggested_title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| extract_url_stem(&parsed));
    let safe_stem = sanitize_name(&stem);

    // 出力ジョブフォルダを排他作成し、RAII ガードを張る (エラー / cancel で掃除)。
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let output_folder =
        create_exclusive_dir(output_directory, &format!("{safe_stem}-mp3-{timestamp}"))?;
    let mut output_guard = TempDirGuard::arm(output_folder.clone());

    // ffmpeg stderr 由来エラーを redact + 認証マーカー付与する共通クロージャ。
    let map_ffmpeg_error = |message: String| -> String {
        if message == CANCEL_MESSAGE {
            message
        } else {
            let redacted = redact_url_queries(&message);
            if looks_like_http_auth_failure(&redacted) {
                format!("{AUTH_REQUIRED_MARKER}{redacted}")
            } else {
                redacted
            }
        }
    };

    if let Some(minutes) = split_minutes {
        // 分割併用: ストリームで単一の正規化済み mp3 を作業ファイルへ書き出し、ローカルで
        // 並列分割する (上記「分割併用時の方式」の根拠どおり)。作業 mp3 は output_folder
        // 内に置き、分割成功後に削除する (TempDirGuard が掃除対象に含む)。
        let work_mp3 = output_folder.join(format!("{safe_stem}.streamed.mp3"));
        let mut stream_args = vec![
            OsString::from("-y"),
            OsString::from("-nostdin"),
            OsString::from("-headers"),
            OsString::from(&headers),
            OsString::from("-i"),
            OsString::from(&url_string),
        ];
        stream_args.extend(base_audio_args(optimize_for_size));
        stream_args.extend([
            OsString::from("-progress"),
            OsString::from("pipe:1"),
            OsString::from("-nostats"),
            work_mp3.as_os_str().into(),
        ]);
        run_ffmpeg_with_cancel(
            progress,
            &ffmpeg,
            stream_args,
            duration_seconds,
            "変換中",
            "URLを直接読み込みながらMP3化しています。",
            cancel,
        )
        .map_err(map_ffmpeg_error)?;

        // Speed-3 (二重エンコード解消): work_mp3 は base_audio_args で既に正規化済み
        // (44100Hz/2ch/メタ除去) なので、run_split_conversion (libmp3lame 再エンコード)
        // ではなく、yt-dlp 経路 (segment_mp3_copy_with_cancel) と同じ build_segment_copy_args
        // による `-c copy` 単一 ffmpeg 分割を使う。stream copy は再エンコードよりはるかに
        // 軽い処理のため、マルチプロセス並列 (Speed-1) の出番自体がなくなる。
        let template = output_folder.join("segment-%03d.mp3");
        let copy_args = build_segment_copy_args(&work_mp3, minutes, &template);
        run_ffmpeg_with_cancel(
            progress,
            &ffmpeg,
            copy_args,
            duration_seconds,
            "分割中",
            "MP3を再エンコードせずに分割しています。",
            cancel,
        )
        .map_err(map_ffmpeg_error)?;

        // 作業 mp3 を削除 (成果物は segment-*.mp3 のみ)。
        let _ = fs::remove_file(&work_mp3);
    } else {
        // 分割なし: URL を直接 ffmpeg へ入力し、出力 mp3 を 1 パスで生成する
        // (DL と変換が重なり一時ファイル不要)。
        let output_path = output_folder.join(format!("{safe_stem}.mp3"));
        let mut args = vec![
            OsString::from("-y"),
            OsString::from("-nostdin"),
            OsString::from("-headers"),
            OsString::from(&headers),
            OsString::from("-i"),
            OsString::from(&url_string),
        ];
        args.extend(base_audio_args(optimize_for_size));
        args.extend([
            OsString::from("-progress"),
            OsString::from("pipe:1"),
            OsString::from("-nostats"),
            output_path.as_os_str().into(),
        ]);
        run_ffmpeg_with_cancel(
            progress,
            &ffmpeg,
            args,
            duration_seconds,
            "変換中",
            "URLを直接読み込みながらMP3化しています。",
            cancel,
        )
        .map_err(map_ffmpeg_error)?;
    }

    progress(&Progress {
        percent: 100,
        step: "完了".into(),
        detail: "変換が完了しました。".into(),
        current_seconds: duration_seconds,
        total_seconds: duration_seconds,
    })?;

    let summary = summarize_outputs(output_folder, duration_seconds)?;
    output_guard.disarm();
    Ok(summary)
}

/// `future` の完了を待ちつつ、cancel トークンを [`DIRECT_DOWNLOAD_CANCEL_POLL_INTERVAL`]
/// 粒度でポーリングする (Slice I-1c 指摘1)。
///
/// cancel が先に立てば `future` を **drop** して `Err(CANCEL_MESSAGE)` を返す。
/// reqwest は子プロセスを持たないため、進行中の `send()` / body 受信 future を
/// drop すれば HTTP 接続が即座に閉じられ、connect/read timeout (30s/120s) を待たず
/// ~100ms 以内で中断できる。`cancel` が `None` の場合は `future` の結果を
/// そのまま返す (ポーリングのオーバーヘッドも持たない = 従来挙動)。
async fn await_or_cancel<F, T>(future: F, cancel: Cancel<'_>) -> Result<T, String>
where
    F: std::future::Future<Output = T>,
{
    match cancel {
        // 後方互換 / 正常系: cancel 無しなら素直に await (追加コスト無し)。
        None => Ok(future.await),
        Some(flag) => {
            // 既に立っている場合は future を起動せず即中断。
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(CANCEL_MESSAGE.to_string());
            }
            tokio::pin!(future);
            loop {
                tokio::select! {
                  // bias を future 側に寄せず、cancel ポーリングが定期的に走るようにする。
                  output = &mut future => return Ok(output),
                  _ = tokio::time::sleep(DIRECT_DOWNLOAD_CANCEL_POLL_INTERVAL) => {
                    if flag.load(std::sync::atomic::Ordering::SeqCst) {
                      // future を drop (= 接続中断) してから中断エラーを返す。
                      return Err(CANCEL_MESSAGE.to_string());
                    }
                  }
                }
            }
        }
    }
}

/// URL からメディアをダウンロードして一時ファイルを作り、そのパスを返す。
///
/// HLS の場合は内部で `spawn_blocking` 相当の処理が必要だが、core 側は
/// 非同期 fn として HLS 判定後に [`download_hls_blocking`] を呼ぶ。
/// `cookie` / `referer` は呼び出し側が解決済みの値を渡す。
pub async fn download_media<R>(
    resolver: &R,
    url: String,
    cookie: Option<String>,
    referer: Option<String>,
    progress: ProgressFn<'_>,
) -> Result<String, String>
where
    R: BinaryResolver + ?Sized,
{
    // 後方互換: cancel トークン無し (None)。MCP / CLI 既存呼び出しは挙動不変。
    download_media_with_cancel(resolver, url, cookie, referer, progress, None).await
}

/// [`download_media`] の協調的キャンセル対応版 (Slice I-1 reviewer fix / 指摘2)。
///
/// `cancel` に `Some(flag)` を渡すと、直リンク DL は
/// (a) `send()` の接続確立 await、(b) body 受信ループの各 `next()` await を
/// [`await_or_cancel`] で cancel ポーリングと race させる。フラグが立てば進行中の
/// future を drop (= HTTP 接続を中断) して即座に中断エラーを返す。これにより
/// **バイトが届かない無音接続** でも connect/read timeout (30s/120s) を待たず
/// ~100ms 以内で停止する (Slice I-1c 指摘1)。reqwest は子プロセスを持たないため
/// kill は不要 (future drop で接続が閉じる)。cancel 検知時は書きかけの一時ファイルを
/// best-effort で削除する。`None` なら従来挙動と同一 (ポーリングコストも持たない)。
///
/// HLS 経路は [`download_hls_blocking_with_cancel`] へ委譲し、cancel トークンを
/// 渡して ffmpeg child を token-cancellable にする (Slice I-1c 指摘2)。
pub async fn download_media_with_cancel<R>(
    resolver: &R,
    url: String,
    cookie: Option<String>,
    referer: Option<String>,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
) -> Result<String, String>
where
    R: BinaryResolver + ?Sized,
{
    let parsed =
        Url::parse(&url).map_err(|error| format!("URL を解析できませんでした: {error}"))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("http または https の URL を指定してください。".into());
    }

    if is_hls_url(&parsed) {
        return download_hls_blocking_with_cancel(
            resolver, parsed, cookie, referer, progress, cancel,
        );
    }

    progress(&Progress {
        percent: 0,
        step: "ダウンロード準備中".into(),
        detail: "接続しています。".into(),
        current_seconds: 0.0,
        total_seconds: 0.0,
    })?;

    // 無限ブロック解消 (Slice B): 接続確立と read(idle) にタイムアウトを設定する。
    // 全体タイムアウトは長尺の正常 DL を誤って打ち切るため使わず、接続確立
    // ([`DIRECT_DOWNLOAD_CONNECT_TIMEOUT`]) と「バイトが届かない無音状態」の
    // read タイムアウト ([`DIRECT_DOWNLOAD_READ_TIMEOUT`]) で守る。進行中の DL は
    // 中断されない。
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(DIRECT_DOWNLOAD_CONNECT_TIMEOUT)
        .read_timeout(DIRECT_DOWNLOAD_READ_TIMEOUT)
        .build()
        .map_err(|error| format!("HTTP クライアントの初期化に失敗しました: {error}"))?;

    let browser_ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15";
    let default_referer = match parsed.host_str() {
        Some(host) => format!("{}://{}/", parsed.scheme(), host),
        None => format!("{}://", parsed.scheme()),
    };
    let referer_value = referer
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or(default_referer);

    let mut builder = client
        .get(parsed.clone())
        .header(reqwest::header::USER_AGENT, browser_ua)
        .header(reqwest::header::REFERER, &referer_value)
        .header(reqwest::header::ACCEPT, "*/*")
        .header(reqwest::header::ACCEPT_LANGUAGE, "ja,en-US;q=0.9,en;q=0.8");

    let cookie_to_send = cookie
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    if let Some(cookie_value) = cookie_to_send {
        builder = builder.header(reqwest::header::COOKIE, cookie_value);
    }

    // Slice I-1c 指摘1: send() の接続確立 await を cancel ポーリングと race させる。
    // 無音ホスト (connect が返らない) でも connect timeout (30s) を待たず ~100ms で
    // 中断する。cancel が先に立てば request future を drop して接続を中断する。
    let response = await_or_cancel(builder.send(), cancel)
        .await?
        // 指摘2: reqwest::Error の Display は対象 URL (query 含む) を埋め込む。
        // connect/read timeout の新エラー発火面を Slice B で足したため、core 側でも
        // 署名/期限付き URL の query を redact してから返す (CLI/host は core 文字列を
        // そのまま外へ出す経路があるため)。
        .map_err(|error| redact_url_queries(&format!("URL への接続に失敗しました: {error}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let hint = match status.as_u16() {
            403 => " (Referer チェックや認証クッキーが必要な可能性があります)",
            401 => " (ログイン/認証が必要です)",
            404 => " (URL の有効期限が切れているか、パスが誤っています)",
            _ => "",
        };
        let message = format!("サーバーがエラーを返しました: HTTP {status}{hint}");
        // A18: 401 / 403 は認証起因。上位 (host convert_url / MCP) が型判定して
        // authRequired フローへ合流できるよう、機械可読マーカーを文言先頭に付ける。
        // yt-dlp 経路の AuthRequired 分類と意味的に整合させる。
        if matches!(status.as_u16(), 401 | 403) {
            return Err(format!("{AUTH_REQUIRED_MARKER}{message}"));
        }
        return Err(message);
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        });

    let extension = infer_extension(&parsed, content_type.as_deref()).ok_or_else(|| {
        "対応していない形式です。MP4 / MOV / M4V / MP3 / WEBM / M4A の直リンクを指定してください。"
            .to_string()
    })?;

    let total_bytes = response.content_length();

    let stem = extract_url_stem(&parsed);
    let safe_stem = sanitize_name(&stem);

    let temp_base = std::env::temp_dir().join("revmagic-downloads");
    let timestamp = Local::now().format("%Y%m%d-%H%M%S-%f").to_string();
    // 共通ヘルパで `<ts>` サブディレクトリを排他作成する (create_dir_all は親
    // (temp_base) の用意にのみ使い、candidate 自体は fs::create_dir で排他作成)。
    // これにより以下でアームする TempDirGuard の対象が確実に新規・排他になる。
    let temp_dir = create_exclusive_dir(&temp_base, &timestamp)?;
    // A5/A6: temp_dir 作成直後に RAII ガードを張る。chunk stream / write / flush /
    // progress エラー、および cancel の全経路で Drop が temp_dir を掃除し、部分
    // ファイル / 空フォルダを残さない。成功時のみ disarm する。
    let mut temp_guard = TempDirGuard::arm(temp_dir.clone());
    let temp_file = temp_dir.join(format!("{safe_stem}.{extension}"));

    let mut file = tokio::fs::File::create(&temp_file)
        .await
        .map_err(|error| format!("一時ファイルを作成できませんでした: {error}"))?;

    progress(&Progress {
        percent: 1,
        step: "ダウンロード中".into(),
        detail: format_download_detail(0, total_bytes),
        current_seconds: 0.0,
        total_seconds: total_bytes.unwrap_or(0) as f64,
    })?;

    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_percent: u8 = 0;

    loop {
        // 指摘1 (Slice I-1c): 各 next() await を cancel ポーリングと race させる。
        // body が無音 (サーバがバイトを送らない) でも read timeout (120s) を待たず
        // ~100ms で中断できる。cancel 検知時は stream future が drop され接続が閉じる。
        let next = match await_or_cancel(stream.next(), cancel).await {
            Ok(next) => next,
            Err(cancel_message) => {
                // 後始末は temp_guard に一本化する。return 時のローカル drop は逆順
                // (file/stream が temp_guard より後に宣言) なので、接続・FD が閉じてから
                // temp_guard の Drop が temp_dir を remove_dir_all する (順序維持)。
                return Err(cancel_message);
            }
        };
        let Some(chunk) = next else { break };

        // 指摘2 (既存): stream エラーも reqwest::Error 経由で URL を含み得るため redact。
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(redact_url_queries(&format!(
                    "ダウンロードに失敗しました: {error}"
                )));
            }
        };
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("一時ファイルへの書き込みに失敗しました: {error}"))?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);

        let percent = match total_bytes {
            Some(total) if total > 0 => ((downloaded.saturating_mul(100)) / total).min(99) as u8,
            _ => 50,
        };

        if percent > last_percent || total_bytes.is_none() {
            last_percent = percent;
            progress(&Progress {
                percent: percent.max(1),
                step: "ダウンロード中".into(),
                detail: format_download_detail(downloaded, total_bytes),
                current_seconds: downloaded as f64,
                total_seconds: total_bytes.unwrap_or(downloaded) as f64,
            })?;
        }
    }

    file.flush()
        .await
        .map_err(|error| format!("書き込みの反映に失敗しました: {error}"))?;
    drop(file);

    progress(&Progress {
        percent: 100,
        step: "ダウンロード完了".into(),
        detail: "変換を開始します。".into(),
        current_seconds: downloaded as f64,
        total_seconds: total_bytes.unwrap_or(downloaded) as f64,
    })?;

    // 成功: temp_file は呼び出し側が変換後に cleanup_download_temp で消す。ここで
    // 消さないようガードを disarm する (disarm 漏れ = 成果物の誤削除に注意)。
    temp_guard.disarm();
    Ok(temp_file.to_string_lossy().to_string())
}

/// A6: 直リンク / HLS の DL 一時ファイルとその専用一時ディレクトリを後始末する。
///
/// `download_media` / `download_hls_blocking` は一時ファイルを
/// `$TMPDIR/revmagic-downloads/<timestamp>/<name>.<ext>` という **1 ファイル専用の
/// サブディレクトリ** に作って返す。従来、呼び出し側 (host / MCP) は変換後にファイル
/// だけを `remove_file` し、その親サブディレクトリ (`<timestamp>/`) が空のまま残置して
/// いた (正常完了時のディレクトリリーク)。
///
/// この関数はファイルを削除したうえで、親が revmagic-downloads 配下の専用サブ
/// ディレクトリであれば **空になった時点でそれも削除** する (best-effort)。`<timestamp>/`
/// は 1 ファイル専用なので空になれば安全に消せる。revmagic-downloads 配下でない親は
/// 触らない (誤って利用者のフォルダを消さない安全弁)。
///
/// 呼び出し側は変換成功時・失敗時いずれもこれを呼べる (B/C 経路が消費する)。
pub fn cleanup_download_temp(downloaded_path: &str) {
    let path = Path::new(downloaded_path);
    let _ = fs::remove_file(path);

    if let Some(parent) = path.parent() {
        // 親が `.../revmagic-downloads/<...>` 形式 (= 専用一時サブディレクトリ) のときだけ
        // 削除を試みる。revmagic-downloads ディレクトリ自体やそれ以外の場所は触らない。
        let is_dedicated_temp = parent
            .parent()
            .and_then(|grandparent| grandparent.file_name())
            .map(|name| name == std::ffi::OsStr::new("revmagic-downloads"))
            .unwrap_or(false);
        if is_dedicated_temp {
            // 空のときだけ消える `remove_dir` を使う (他ファイルがあれば残す)。
            let _ = fs::remove_dir(parent);
        }
    }
}

/// A5/A6: disarm 可能な一時ディレクトリ RAII ガード。
///
/// `arm` で守るディレクトリを与えると、ガードが drop される全経路 (早期 `?`
/// return / cancel / panic 巻き戻し) で `remove_dir_all` を **best-effort** で実行し、
/// エラー / cancel 経路に部分ファイルや空フォルダを残さない。成功パスでは
/// [`Self::disarm`] を呼んでガードを無効化し、誤って成果物 (呼び出し側へ返す
/// temp_file や、ユーザー出力フォルダ) を削除しないようにする。
///
/// 既存 [`ytdlp::TempCookieFile`] の Drop と同方針で、削除失敗時もログ / パスを
/// 出さない (秘密 / パス露出回避)。
struct TempDirGuard {
    path: Option<PathBuf>,
}

impl TempDirGuard {
    /// `path` を守るガードを構築する (drop 時に `remove_dir_all`)。
    fn arm(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// ガードを無効化する (成功パス用)。以降 drop しても削除しない。
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // best-effort 削除。失敗してもログ / パスは出さない。
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// A18: ffmpeg の stderr 文言が HTTP 401 / 403 (認証起因) を示すか判定する。
///
/// ffmpeg は HTTP ステータスを `Server returned 401 Unauthorized` /
/// `HTTP error 403 Forbidden` 等の文言で出す。HLS 本体取得が認証で弾かれたケースを
/// 直リンク経路と同じ `AUTH_REQUIRED_MARKER` 付与へ合流させるために使う。
fn looks_like_http_auth_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
}

fn infer_extension(url: &Url, content_type: Option<&str>) -> Option<String> {
    let path_ext = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|value| !value.is_empty()))
        .and_then(|name| {
            PathBuf::from(name)
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase())
        });

    if let Some(ext) = path_ext {
        if ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
            return Some(ext);
        }
    }

    match content_type? {
        "video/mp4" => Some("mp4".into()),
        "video/quicktime" => Some("mov".into()),
        "video/x-m4v" => Some("m4v".into()),
        "video/webm" => Some("webm".into()),
        "audio/mpeg" | "audio/mp3" => Some("mp3".into()),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => Some("m4a".into()),
        _ => None,
    }
}

fn extract_url_stem(url: &Url) -> String {
    let segment = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|value| !value.is_empty()))
        .unwrap_or("download")
        .to_string();

    let stem = PathBuf::from(&segment)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "download".to_string());

    stem
}

fn format_download_detail(downloaded: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!(
            "{} / {} 取得中",
            format_bytes(downloaded),
            format_bytes(total)
        ),
        _ => format!("{} 取得中", format_bytes(downloaded)),
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index < UNITS.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{} {}", bytes, UNITS[index])
    } else {
        format!("{:.1} {}", value, UNITS[index])
    }
}

/// 入力ファイルの長さ (秒) を ffprobe で取得する。
pub fn inspect_media<R: BinaryResolver + ?Sized>(
    resolver: &R,
    input_path: &str,
) -> Result<f64, String> {
    let input = PathBuf::from(input_path);

    if !input.exists() {
        return Err("入力ファイルが見つかりません。".into());
    }

    let ffprobe = resolver.resolve("ffprobe")?;
    read_duration_seconds(&ffprobe, &input)
}

/// ローカルファイルを MP3 へ変換する (同期・blocking)。
///
/// 呼び出し側は `spawn_blocking` 等でブロッキング実行することを想定する。
pub fn convert_media_blocking<R: BinaryResolver + ?Sized>(
    resolver: &R,
    request: ConversionRequest,
    progress: ProgressFn<'_>,
) -> Result<ConversionSummary, String> {
    // 後方互換: cancel トークン無し (None)。MCP / CLI 既存呼び出しは挙動不変。
    convert_media_blocking_with_cancel(resolver, request, progress, None)
}

/// [`convert_media_blocking`] の協調的キャンセル対応版 (Slice I-1 reviewer fix)。
///
/// `cancel` に `Some(flag)` を渡すと、内部の ffmpeg 変換 (単一 / 分割) が行読み
/// ループ各反復でフラグを確認し、立っていれば ffmpeg child (プロセスグループ) を
/// kill して即座に中断する (指摘1 / 指摘2)。`None` なら従来挙動と同一。
pub fn convert_media_blocking_with_cancel<R: BinaryResolver + ?Sized>(
    resolver: &R,
    request: ConversionRequest,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
) -> Result<ConversionSummary, String> {
    let input = PathBuf::from(&request.input_path);
    let output_directory = PathBuf::from(&request.output_directory);

    if !input.exists() {
        return Err("入力ファイルが見つかりません。".into());
    }

    if !output_directory.exists() {
        return Err("保存先フォルダが見つかりません。".into());
    }

    progress(&Progress {
        percent: 0,
        step: "準備中".into(),
        detail: "変換エンジンを準備しています。".into(),
        current_seconds: 0.0,
        total_seconds: 0.0,
    })?;

    let ffmpeg = resolver.resolve("ffmpeg")?;
    let ffprobe = resolver.resolve("ffprobe")?;

    progress(&Progress {
        percent: 3,
        step: "解析中".into(),
        detail: "ファイルの長さを取得しています。".into(),
        current_seconds: 0.0,
        total_seconds: 0.0,
    })?;

    let duration_seconds = read_duration_seconds(&ffprobe, &input)?;
    // create_output_folder は `<output_directory>/<stem>-mp3-<ts>/` という新規専用
    // ジョブフォルダを作って返す (output_directory 自体や他ファイルではない)。
    let output_folder = create_output_folder(&input, &output_directory)?;
    // A5/A6: ジョブフォルダ作成直後に RAII ガードを張る。run_split_conversion /
    // run_single_conversion / 末尾 progress / summarize_outputs のいずれが失敗・
    // cancel しても Drop が専用ジョブフォルダを掃除し、partial / 空フォルダを
    // ユーザー出力先に残さない。output_directory 自体や他ファイルは削除対象に
    // 含めない (create_output_folder が必ず新規空フォルダを返す)。成功時のみ
    // disarm する。
    let mut output_guard = TempDirGuard::arm(output_folder.clone());
    let optimize_for_size = request.optimize_for_size;

    if let Some(split_minutes) = request.split_minutes {
        // A14: 範囲検証 (1..=MAX_SPLIT_MINUTES)。秒変換は run_split_conversion 内で
        // saturating_mul を使う。ここでは早期に範囲外を弾く。
        let _ = split_minutes_to_seconds(split_minutes)?;

        run_split_conversion(
            progress,
            &ffmpeg,
            &input,
            &output_folder,
            split_minutes,
            optimize_for_size,
            duration_seconds,
            cancel,
        )?;
    } else {
        run_single_conversion(
            progress,
            &ffmpeg,
            &ffprobe,
            &input,
            &output_folder,
            optimize_for_size,
            duration_seconds,
            cancel,
        )?;
    }

    progress(&Progress {
        percent: 100,
        step: "完了".into(),
        detail: "変換が完了しました。".into(),
        current_seconds: duration_seconds,
        total_seconds: duration_seconds,
    })?;

    // summarize_outputs が成功して初めて成果物として確定する。失敗時は partial が
    // 残らないようガードを armed のままにし、成功時のみ disarm する。
    let summary = summarize_outputs(output_folder, duration_seconds)?;
    output_guard.disarm();
    Ok(summary)
}

fn read_duration_seconds(ffprobe: &Path, input: &Path) -> Result<f64, String> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(input)
        // SIGTTIN 根絶 (端末 stdin を継承させない)。
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("ffprobe の実行に失敗しました: {error}"))?;

    if !output.status.success() {
        return Err(stderr_message(
            "メディアの長さを取得できませんでした。",
            &output.stderr,
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("メディアの長さを解析できませんでした: {error}"))
}

/// ffmpeg/直メディア経路の生成物を隔離する出力フォルダ
/// `parent` 直下に `base_name`（衝突時は `base_name-2`, `base_name-3`, ...）という
/// **新規・空・排他なディレクトリ**を作って返す共通ヘルパ。
///
/// ## 排他作成の不変条件
/// 返すディレクトリは必ず `fs::create_dir`（既存なら `AlreadyExists`）で**今まさに
/// 新規作成**したものに限る。`fs::create_dir_all` は既存ディレクトリでも成功して
/// しまい、呼び出し側でアームされる [`TempDirGuard`] が先行ジョブの成果物や他者の
/// データごと `remove_dir_all` してしまう退行を招くため、**最終作成には絶対に
/// 使わない**。`create_dir_all` は親 (`parent`) が無い場合の親の用意にのみ使い、
/// その後は必ず `fs::create_dir` で candidate を排他作成し直す。
///
/// 衝突 (`AlreadyExists`) / 親不在 (`NotFound`) のいずれもサフィックスを足した
/// 有限リトライ (最大 1000 回) で解消し、無限ループしない。それ以外のエラーは
/// 即時 `Err`。エラー文言にはパスを含めない (秘密/パス漏洩防止)。
pub(crate) fn create_exclusive_dir(parent: &Path, base_name: &str) -> Result<PathBuf, String> {
    // 初回の親不在で一度だけ親を用意したかどうか (無限の create_dir_all 再試行防止)。
    let mut parent_prepared = false;
    for suffix in 0..1000 {
        let candidate = if suffix == 0 {
            parent.join(base_name)
        } else {
            parent.join(format!("{base_name}-{suffix}"))
        };
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            // 既存 (同一秒の連続/並列実行等) ならサフィックスを足して排他作成リトライ。
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // 親が無い場合は **親のみ** create_dir_all で用意し、candidate 自体は次周回の
            // fs::create_dir で必ず排他作成する (candidate を create_dir_all しない)。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !parent_prepared => {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("出力フォルダを作成できませんでした: {error}"))?;
                parent_prepared = true;
                // 同じ suffix を排他作成し直すため、suffix を進めずに再試行する。
                let candidate = if suffix == 0 {
                    parent.join(base_name)
                } else {
                    parent.join(format!("{base_name}-{suffix}"))
                };
                match fs::create_dir(&candidate) {
                    Ok(()) => return Ok(candidate),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        return Err(format!("出力フォルダを作成できませんでした: {error}"));
                    }
                }
            }
            Err(error) => {
                return Err(format!("出力フォルダを作成できませんでした: {error}"));
            }
        }
    }
    Err("出力フォルダを作成できませんでした (一意な名前を確保できません)。".to_string())
}

/// `<output_root>/<stem>-mp3-<ts>/` を作る。
///
/// **必ず新規・空・排他なフォルダを返す**。タイムスタンプは秒粒度なので、同一 stem・
/// 同一 output_root・同一秒で連続呼び出しすると衝突し得る。共通ヘルパ
/// [`create_exclusive_dir`] が `fs::create_dir`（既存なら `AlreadyExists`）+
/// サフィックスリトライで一意性・排他性を保証するため、呼び出し側でアームされる
/// [`TempDirGuard`] が先行ジョブの成功成果物を `remove_dir_all` してしまう退行を防ぐ。
fn create_output_folder(input: &Path, output_root: &Path) -> Result<PathBuf, String> {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("audio");

    let safe_stem = sanitize_name(stem);
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    create_exclusive_dir(output_root, &format!("{safe_stem}-mp3-{timestamp}"))
}

/// ファイル名 (1 セグメント) の最大バイト長。多くの FS の per-component 上限が
/// 255 byte なので、拡張子 (`.mp3` 等) を足しても収まるよう余裕を見て 200 byte に
/// 制限する (A16)。
const SANITIZE_NAME_MAX_BYTES: usize = 200;

/// ファイル名 1 セグメントを安全化する (A16 強化)。
///
/// 従来は OS 予約文字を `_` へ置換するだけだったが、以下を追加する:
/// - **制御文字** (TAB / 改行 / NUL 等) を `_` へ置換する (端末汚染・行注入防止)。
/// - パスセパレータ / 予約文字を `_` へ置換する (従来どおり)。
/// - 連続 / 先頭末尾の `.` を畳み、`.` / `..` のみの名前 (親ディレクトリ参照や
///   隠し・無名ファイル化) を安全な既定名へ正規化する (パストラバーサル防止)。
/// - バイト長を [`SANITIZE_NAME_MAX_BYTES`] に制限する (超長名による FS エラー防止)。
///   切り詰めは char 境界で行い、不正な UTF-8 を作らない。
fn sanitize_name(name: &str) -> String {
    // 1. 予約文字 / 制御文字を `_` へ。
    let replaced: String = name
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other if other.is_control() => '_',
            other => other,
        })
        .collect();

    // 2. 先頭・末尾の `.` / 空白を除去する。これにより `..` や `.` 単独、
    //    先頭ドットの隠しファイル化を防ぐ。
    let trimmed = replaced.trim_matches(|c: char| c == '.' || c.is_whitespace());

    // 3. 空 (元が `.` / `..` / 空白のみ等) になったら安全な既定名へ。
    let normalized = if trimmed.is_empty() { "audio" } else { trimmed };

    // 4. バイト長を上限まで char 境界で切り詰める。
    if normalized.len() <= SANITIZE_NAME_MAX_BYTES {
        return normalized.to_string();
    }
    let mut end = SANITIZE_NAME_MAX_BYTES;
    while end > 0 && !normalized.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &normalized[..end];
    // 切り詰めで末尾が `.` / 空白になった場合も再度トリムする (空なら既定名)。
    let cut = cut.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if cut.is_empty() {
        "audio".to_string()
    } else {
        cut.to_string()
    }
}

fn base_audio_args(optimize_for_size: bool) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-vn"),
        OsString::from("-map_metadata"),
        OsString::from("-1"),
        OsString::from("-c:a"),
        OsString::from("libmp3lame"),
    ];
    if optimize_for_size {
        args.extend([
            OsString::from("-b:a"),
            OsString::from("64k"),
            OsString::from("-ar"),
            OsString::from("24000"),
            OsString::from("-ac"),
            OsString::from("1"),
            OsString::from("-threads"),
            OsString::from("1"),
        ]);
    } else {
        args.extend([
            OsString::from("-q:a"),
            OsString::from("2"),
            OsString::from("-ar"),
            OsString::from("44100"),
            OsString::from("-ac"),
            OsString::from("2"),
        ]);
    }
    args
}

/// 入力ストリームの copy fast-path 適格判定に使う ffprobe 計測結果 (Slice E)。
///
/// 全フィールドを「確実に取得できた」場合のみ [`is_copy_eligible`] が真を返す。
/// ffprobe が失敗する / 値が空 / パースできない場合は **不明** 扱いとし、
/// 保守的に再エンコード経路へ倒す (疑わしきは従来経路)。
struct CopyProbe {
    /// 先頭オーディオストリームの codec_name (例: "mp3")。空なら不明。
    codec_name: String,
    /// sample_rate (Hz)。0 なら不明。
    sample_rate: u32,
    /// channels。0 なら不明。
    channels: u32,
    /// format/stream に除去対象タグが 1 つでも存在するか。
    ///
    /// copy 引数にも `-map_metadata -1` を付けたため metadata 等価性はそれで担保されるが、
    /// 本フラグは **保守的に維持** する: 除去対象タグがある入力は copy 適格にせず従来
    /// 再エンコード経路へ倒す (negative ケース a の挙動を保ち、曖昧な入力で copy を発火させ
    /// ない)。判定できない場合は安全側に `true` (タグ有り扱い)。
    has_tags: bool,
}

impl CopyProbe {
    /// copy fast-path の安全条件 (mp3 / 44100Hz / 2ch / タグ無し) を **すべて**
    /// 満たすときだけ真。1 つでも不一致・不明なら偽 (= 従来 libmp3lame 経路)。
    fn is_copy_eligible(&self) -> bool {
        self.codec_name == "mp3"
            && self.sample_rate == 44100
            && self.channels == 2
            && !self.has_tags
    }
}

/// ffprobe で入力の copy 適格性を計測する (Slice E)。
///
/// 取得に失敗した場合や値が曖昧な場合は、保守原則に従い「適格でない」方向
/// (codec 空 / sample_rate 0 / channels 0 / has_tags=true) の値を返す。これにより
/// [`CopyProbe::is_copy_eligible`] は偽となり、従来 libmp3lame 経路を通る。
fn probe_copy_eligibility(ffprobe: &Path, input: &Path) -> CopyProbe {
    // 先頭オーディオストリームの codec_name / sample_rate / channels を取得。
    let stream_probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,sample_rate,channels",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(input)
        // SIGTTIN 根絶 (端末 stdin を継承させない)。
        .stdin(Stdio::null())
        .output();

    let mut codec_name = String::new();
    let mut sample_rate = 0u32;
    let mut channels = 0u32;

    if let Ok(output) = stream_probe {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Some(value) = line.strip_prefix("codec_name=") {
                    codec_name = value.trim().to_string();
                } else if let Some(value) = line.strip_prefix("sample_rate=") {
                    sample_rate = value.trim().parse().unwrap_or(0);
                } else if let Some(value) = line.strip_prefix("channels=") {
                    channels = value.trim().parse().unwrap_or(0);
                }
            }
        }
    }

    // タグ残留検知: format/stream の全 tag を出力させ、**除去対象タグ** の有無を見る。
    //
    // 現挙動 (`-map_metadata -1`) と等価にするのが目的。重要な事実: ffmpeg の mp3
    // muxer/encoder は `-map_metadata -1` を付けても出力に `encoder` タグ
    // (`TAG:encoder=Lavc...` / `Lavf...`) を必ず書き戻す。つまり `encoder` は
    // **再エンコード経路でも残るタグ** であり「除去対象タグ」ではない。よって copy
    // 適格判定でも `encoder` は無視する (これを除去対象に数えると、いかなる mp3 でも
    // copy が発火せず fast-path が死ぬ)。
    //
    // `encoder` 以外の非空タグ (title/artist/comment/album 等) が 1 つでもあれば、
    // それは `-map_metadata -1` で消える=copy では残ってしまうタグなので、適格判定は
    // タグ有り (has_tags=true) とし従来再エンコード経路へ倒す。
    // ffprobe 失敗 / 異常終了時は不明 → 保守的にタグ有り扱い。
    let has_tags = match Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format_tags:stream_tags",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(input)
        // SIGTTIN 根絶 (端末 stdin を継承させない)。
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // `default=noprint_wrappers=1` は `TAG:key=value` 形式で 1 行ずつ出力する。
            // 除去対象タグ = キーが `encoder` 以外で値が非空のもの。
            stdout.lines().any(|line| {
                // 先頭の `TAG:` プレフィックスを剥がす (無ければそのまま)。
                let line = line.strip_prefix("TAG:").unwrap_or(line);
                match line.split_once('=') {
                    Some((key, value)) => {
                        let key = key.trim().to_ascii_lowercase();
                        !value.trim().is_empty() && key != "encoder"
                    }
                    None => false,
                }
            })
        }
        // 取得失敗 / 異常終了は不明 → 安全側 (タグ有り扱い)。
        _ => true,
    };

    CopyProbe {
        codec_name,
        sample_rate,
        channels,
        has_tags,
    }
}

/// copy fast-path 用の ffmpeg 引数を組み立てる (Slice E, テスト可能なように分離・`pub`)。
///
/// 入力が **既に最終形の MP3 (mp3 / 44100Hz / 2ch / メタ無し)** で optimize/split が
/// 無いと判定されたときだけ使う。`-c copy` で stream copy し、`libmp3lame` 再エンコードや
/// `base_audio_args` (正規化引数) を一切含めない。
///
/// ## probe 対象と copy 対象の一致 (`-map 0:a:0`)
/// `probe_copy_eligibility` は ffprobe `-select_streams a:0` で **a:0** だけを検査する。
/// copy 実行でも同じ a:0 を選ぶことを保証するため **`-map 0:a:0`** を明示する。これが無いと
/// multi-audio 入力 (a:0=mp3/44100/2ch + a:1=別codec) で ffmpeg の既定 audio selection が
/// a:1 を選び、MP3 muxer が非mp3 stream を mux できず失敗しうる。probe 対象=copy 対象を
/// 明示一致させることで multi-audio でも a:0 を正しく copy する。
///
/// ## metadata 等価性 (`-map_metadata -1`)
/// 従来再エンコード経路は `base_audio_args` の global `-map_metadata -1` で入力 metadata を
/// 落とし、ffmpeg 側の値 (`encoder` 等) に置換する。copy でも同じ `-map_metadata -1` を付け、
/// 入力由来 metadata を常に落として**現挙動と等価**にする (適格判定の `encoder` allowlist に
/// 依存せず、任意の `encoder`/`TSSE` 値が出力へ残らないことを保証)。stream copy は維持。
///
/// `pub` 公開は、Slice E の引数レベル検証 (`tests/copy_fastpath.rs`) が
/// 「copy 経路で `-c copy` が使われ libmp3lame が使われない / `-map 0:a:0` / `-map_metadata -1`」
/// を deterministic に確認するため。
pub fn build_single_copy_args(input: &Path, output_path: &Path) -> Vec<OsString> {
    vec![
        OsString::from("-y"),
        // -nostdin: ffmpeg が stdin を対話的に読まないことを明示 (SIGTTIN ハング防止)。
        OsString::from("-nostdin"),
        OsString::from("-i"),
        input.as_os_str().into(),
        // probe 対象 (a:0) と copy 対象を明示一致させる (multi-audio で a:1 を拾わない)。
        OsString::from("-map"),
        OsString::from("0:a:0"),
        OsString::from("-vn"),
        // copy fast-path の要: 再エンコードせず stream copy する。
        OsString::from("-c"),
        OsString::from("copy"),
        // 従来再エンコード経路 (base_audio_args) と等価に入力 metadata を落とす。
        OsString::from("-map_metadata"),
        OsString::from("-1"),
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        output_path.as_os_str().into(),
    ]
}

/// 従来の libmp3lame 単一変換引数を組み立てる (Slice E, テスト可能なように分離・`pub`)。
///
/// `base_audio_args` (`libmp3lame -ar 44100 -ac 2 -map_metadata -1`) を含む再エンコード
/// 経路。copy 適格でない入力 (非mp3 / 非44100 / 非2ch / タグ有り / optimize) に使う。
///
/// `pub` 公開は Slice E の引数レベル検証用 (negative 5 ケースで本経路に倒れることを確認)。
pub fn build_single_reencode_args(
    input: &Path,
    output_path: &Path,
    optimize_for_size: bool,
) -> Vec<OsString> {
    // -nostdin: ffmpeg が stdin を対話的に読まないことを明示 (SIGTTIN ハング防止)。
    let mut args = vec![
        OsString::from("-y"),
        OsString::from("-nostdin"),
        OsString::from("-i"),
        input.as_os_str().into(),
    ];
    args.extend(base_audio_args(optimize_for_size));
    args.extend([
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        output_path.as_os_str().into(),
    ]);
    args
}

#[allow(clippy::too_many_arguments)]
fn run_single_conversion(
    progress: ProgressFn<'_>,
    ffmpeg: &Path,
    ffprobe: &Path,
    input: &Path,
    output_folder: &Path,
    optimize_for_size: bool,
    duration_seconds: f64,
    cancel: Cancel<'_>,
) -> Result<(), String> {
    // A12: file_stem が空 / 非UTF-8 / sanitize 後に空だと `.mp3` (隠し・無名ファイル)
    // になり、`extension()` が None を返して summarize の file_count=0 誤報につながる。
    // 空にならないよう sanitize_name を通し (空なら "audio")、安全な基底名を保証する。
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .map(sanitize_name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "audio".to_string());
    let output_path = output_folder.join(format!("{stem}.mp3"));

    // Slice E: 入力が既に最終形 MP3 (mp3/44100/2ch/メタ無し) かつ optimize 無しなら
    // 再エンコード不要 → `-c copy` fast-path。split は呼び出し側で別経路 (run_split_conversion)
    // に分岐済みなのでここには来ない。1 つでも条件不一致 / ffprobe 不明なら従来経路。
    let copy_eligible =
        !optimize_for_size && probe_copy_eligibility(ffprobe, input).is_copy_eligible();

    let args = if copy_eligible {
        build_single_copy_args(input, &output_path)
    } else {
        build_single_reencode_args(input, &output_path, optimize_for_size)
    };

    run_ffmpeg_with_cancel(
        progress,
        ffmpeg,
        args,
        duration_seconds,
        "変換中",
        "MP3を生成しています。",
        cancel,
    )
}

/// 1 セグメントの範囲 (秒)。`-ss start -t len` に対応する。
///
/// `index` は出力連番 (`segment-%03d.mp3` の `%03d`) と一致させるための 0 始まり
/// 通し番号。`start` は入力先頭からのオフセット秒、`len` はそのセグメントの長さ秒。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentRange {
    pub index: usize,
    pub start: f64,
    pub len: f64,
}

/// Speed-1: `duration_seconds` を `segment_seconds` 粒度で **抜け / 重複の無い
/// 連続セグメント** に切り分ける (純関数・テスト可能)。
///
/// ## 連続性 / 全長カバーの保証
/// - セグメント `i` は `[i * segment_seconds, (i+1) * segment_seconds)` を覆う。
/// - 末尾セグメントは残り (`duration - start`) だけの長さに切り詰める。
/// - 全セグメントの `start` は前セグメントの `start + len` と一致し、隙間も重複も
///   生じない。最後の `start + len` は `duration_seconds` に一致する。
///
/// ## 個数
/// `ceil(duration / segment_seconds)`。`duration <= 0` や duration 不明 (0.0) の
/// 場合は 1 セグメント (全体) を返す。`segment_seconds == 0` は呼び出し側で
/// [`split_minutes_to_seconds`] により弾かれているが、防御的に 1 セグメントへ倒す。
///
/// 浮動小数の累積誤差を避けるため `start` は **整数 index × segment_seconds** で
/// 直接計算する (前値への加算を繰り返さない)。
pub fn compute_segment_ranges(duration_seconds: f64, segment_seconds: u64) -> Vec<SegmentRange> {
    // duration 不明 / 非正 / NaN / segment 0 は 1 セグメント (全体) として扱う。
    // `>=` の否定 (`< || NaN`) ではなく、正であることを明示的に判定して NaN も弾く。
    let duration_is_positive = duration_seconds.is_finite() && duration_seconds > 0.0;
    if !duration_is_positive || segment_seconds == 0 {
        return vec![SegmentRange {
            index: 0,
            start: 0.0,
            len: duration_seconds.max(0.0),
        }];
    }
    let seg = segment_seconds as f64;
    // ceil(duration / seg)。浮動小数の僅かな超過で 1 個増えないよう微小イプシロンを引く。
    let count = (((duration_seconds - 1e-6) / seg).floor() as i64 + 1).max(1) as usize;
    let mut ranges = Vec::with_capacity(count);
    for index in 0..count {
        let start = (index as f64) * seg;
        // 末尾は残り時間に切り詰める (全長カバー / 重複なし)。
        let len = (duration_seconds - start).min(seg).max(0.0);
        ranges.push(SegmentRange { index, start, len });
    }
    ranges
}

/// Speed-1: 1 セグメント分の独立 ffmpeg 引数を組み立てる (テスト可能なように分離)。
///
/// `-ss <start>` を **入力前** に置く (`-i` より前) ことで input seek を使い、
/// ffmpeg がデコード対象を `start` 付近まで高速スキップする。`-t <len>` で
/// そのセグメント長だけを出力する。音声は `base_audio_args` で再エンコードする
/// (mp4→mp3 等は copy 不可なので各セグメント並列再エンコードで多コアを活用する)。
///
/// 入力前 `-ss` は音声 (libmp3lame) では十分な精度で、`compute_segment_ranges`
/// が連続・全長カバーになるよう `start`/`len` を与えるため抜け / 重複は生じない。
pub fn build_segment_reencode_args(
    input: &Path,
    range: SegmentRange,
    output_path: &Path,
    optimize_for_size: bool,
) -> Vec<OsString> {
    // -nostdin: ffmpeg が stdin を対話的に読まないことを明示 (SIGTTIN ハング防止)。
    let mut args = vec![
        OsString::from("-y"),
        OsString::from("-nostdin"),
        // 入力前 seek (高速)。秒単位で渡す。
        OsString::from("-ss"),
        OsString::from(format!("{:.6}", range.start)),
    ];
    // len > 0 なら明示的に -t で範囲を確定させる。len <= 0 (異常) のときは -t を付けず
    // 入力末尾まで (防御; 通常起きない)。
    if range.len > 0.0 {
        args.push(OsString::from("-t"));
        args.push(OsString::from(format!("{:.6}", range.len)));
    }
    args.push(OsString::from("-i"));
    args.push(input.as_os_str().into());
    args.extend(base_audio_args(optimize_for_size));
    args.extend([
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        output_path.as_os_str().into(),
    ]);
    args
}

/// Speed-1: 分割を **セグメントごとの独立 ffmpeg** でコア数上限の並列実行にする。
///
/// 単一 ffmpeg の `-f segment` 順次処理 (1 コアで全長を再エンコード) をやめ、
/// [`compute_segment_ranges`] で算出した各セグメントを `-ss/-t` 個別 ffmpeg で
/// 起動し、`min(cores, seg数)` の **有限ワーカー** で並列再エンコードする。これにより
/// mp4→mp3 の再エンコード時間を ~コア数倍 短縮する。
///
/// - 並列度は [`std::thread::available_parallelism`] (失敗時 1) と seg 数の min で
///   上限を切り、無制限 spawn はしない (有限ワーカー + 共有キュー)。
/// - 各ワーカーは [`run_ffmpeg_with_cancel`] で 1 セグメントを変換するため、cancel
///   トークン即応 / child killpg / 進捗パースを維持する。
/// - 進捗は **完了セグメント数ベース** で集約し (個別 ffmpeg の秒進捗は使わない)、
///   メインスレッドからのみ progress コールバックを呼ぶ (Send 制約を満たす)。
/// - cancel / エラー時は共有フラグを立てて未着手セグメントの起動を止める。部分
///   セグメントの後始末は呼び出し側の [`TempDirGuard`] が担う (既存方針を維持)。
///
/// seg 数 1 (duration <= segment / duration 不明) のときは並列の利得が無く、duration
/// 不明時は `-ss/-t` 個別起動が全長カバーを保証できないため、単一 ffmpeg の
/// `-f segment` 経路 ([`run_split_conversion_single_ffmpeg`]) にフォールバックする
/// (既存挙動・全長カバーを保つ)。
#[allow(clippy::too_many_arguments)]
fn run_split_conversion(
    progress: ProgressFn<'_>,
    ffmpeg: &Path,
    input: &Path,
    output_folder: &Path,
    split_minutes: u64,
    optimize_for_size: bool,
    duration_seconds: f64,
    cancel: Cancel<'_>,
) -> Result<(), String> {
    // A14: u64 オーバーフロー防止 (検証は呼び出し側で実施済みだが防御的に saturating)。
    let segment_seconds = split_minutes.saturating_mul(60);
    let ranges = compute_segment_ranges(duration_seconds, segment_seconds);

    // seg 数 1 (= 分割不要 / duration 不明) は単一 ffmpeg segment 経路へ。
    if ranges.len() <= 1 {
        return run_split_conversion_single_ffmpeg(
            progress,
            ffmpeg,
            input,
            output_folder,
            segment_seconds,
            optimize_for_size,
            duration_seconds,
            cancel,
        );
    }

    let total = ranges.len();
    // 並列度: コア数と seg 数の小さい方。available_parallelism 失敗時は 1 に縮退。
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let workers = cores.min(total).max(1);

    progress(&Progress {
        percent: 5,
        step: "変換中".into(),
        detail: format!("MP3を{total}個に並列分割しています。"),
        current_seconds: 0.0,
        total_seconds: duration_seconds,
    })?;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // 共有作業キュー。ワーカーは pop して 1 セグメントを変換する。pop() は末尾から
    // 取るので逆順 (index 降順) に積む = index 昇順で処理されるが、出力ファイル名は
    // range.index で決まるため処理順に依らず連番は正しい。
    let queue: Arc<Mutex<Vec<SegmentRange>>> = {
        let mut v = ranges;
        v.reverse();
        Arc::new(Mutex::new(v))
    };
    // 完了セグメント数 (進捗集約用)。
    let done = Arc::new(AtomicUsize::new(0));
    // いずれかのワーカーが失敗 / cancel を検知したら立て、未着手の起動を止める。
    let aborted = Arc::new(AtomicBool::new(false));
    // 最初に観測した失敗メッセージ (1 つ保持)。
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let optimize = optimize_for_size;
    let ffmpeg_owned = ffmpeg.to_path_buf();
    let input_owned = input.to_path_buf();
    let output_owned = output_folder.to_path_buf();

    // thread::scope で借用ベースの並列を行う (cancel: &AtomicBool をそのまま共有)。
    let scope_result: Result<(), String> = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let done = Arc::clone(&done);
            let aborted = Arc::clone(&aborted);
            let first_error = Arc::clone(&first_error);
            let ffmpeg_owned = ffmpeg_owned.clone();
            let input_owned = input_owned.clone();
            let output_owned = output_owned.clone();
            handles.push(scope.spawn(move || {
                loop {
                    // 中断要求 (失敗 / cancel) が立っていれば新規セグメントを取らない。
                    if aborted.load(Ordering::SeqCst) || ytdlp::is_cancelled(cancel) {
                        return;
                    }
                    let range = {
                        let mut guard = queue.lock().unwrap_or_else(|p| p.into_inner());
                        match guard.pop() {
                            Some(range) => range,
                            None => return,
                        }
                    };
                    let output_path = output_owned.join(format!("segment-{:03}.mp3", range.index));
                    let args =
                        build_segment_reencode_args(&input_owned, range, &output_path, optimize);
                    // 進捗を出さない no-op コールバック。集約進捗はメインスレッドが done を
                    // 見て発火するため、ワーカーからは progress を呼ばない (Send 制約回避)。
                    let noop: ProgressFn<'_> = &|_p: &Progress| Ok(());
                    match run_ffmpeg_with_cancel(
                        noop,
                        &ffmpeg_owned,
                        args,
                        range.len,
                        "変換中",
                        "MP3を分割しています。",
                        cancel,
                    ) {
                        Ok(()) => {
                            done.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(message) => {
                            aborted.store(true, Ordering::SeqCst);
                            let mut slot = first_error.lock().unwrap_or_else(|p| p.into_inner());
                            if slot.is_none() {
                                *slot = Some(message);
                            }
                            return;
                        }
                    }
                }
            }));
        }

        // メインスレッド: 完了数 / cancel / abort を 100ms 粒度で監視し進捗を集約発火。
        let aggregate_result = loop {
            // 外部 cancel: ワーカーに伝播済み (各 run_ffmpeg_with_cancel が killpg) だが、
            // メイン側でも中断要求を立てて早期に抜ける。
            if ytdlp::is_cancelled(cancel) {
                aborted.store(true, Ordering::SeqCst);
                break Err(CANCEL_MESSAGE.to_string());
            }
            let completed = done.load(Ordering::SeqCst);
            // 完了数ベースの集約進捗 (5..=99%)。
            let percent = {
                let frac = completed as f64 / total as f64;
                (5.0 + frac * 94.0).round().clamp(5.0, 99.0) as u8
            };
            if let Err(error) = progress(&Progress {
                percent,
                step: "変換中".into(),
                detail: format!("分割 {completed}/{total} 完了"),
                current_seconds: 0.0,
                total_seconds: duration_seconds,
            }) {
                // progress 側の Err (cancel / channel 切断) でも中断要求を立てる。
                aborted.store(true, Ordering::SeqCst);
                break Err(error);
            }

            if completed >= total {
                break Ok(());
            }
            if aborted.load(Ordering::SeqCst) {
                // ワーカーが失敗を検知。join 後に first_error を返す。
                break Ok(());
            }
            thread::sleep(FFMPEG_POLL_INTERVAL);
        };

        // 全ワーカーの終了を待つ (scope も join するが、エラー判定のため明示)。
        for handle in handles {
            let _ = handle.join();
        }

        // 集約ループ自体が cancel / progress-Err で抜けたらそれを優先して返す。
        aggregate_result?;

        // ワーカー由来のエラー / cancel を反映する。
        if let Some(message) = first_error.lock().unwrap_or_else(|p| p.into_inner()).take() {
            return Err(message);
        }
        if ytdlp::is_cancelled(cancel) {
            return Err(CANCEL_MESSAGE.to_string());
        }
        Ok(())
    });

    scope_result
}

/// Speed-1 フォールバック: 単一 ffmpeg の `-f segment` でセグメント分割する
/// (旧 `run_split_conversion` 本体)。seg 数 1 / duration 不明のときに使う。
#[allow(clippy::too_many_arguments)]
fn run_split_conversion_single_ffmpeg(
    progress: ProgressFn<'_>,
    ffmpeg: &Path,
    input: &Path,
    output_folder: &Path,
    segment_seconds: u64,
    optimize_for_size: bool,
    duration_seconds: f64,
    cancel: Cancel<'_>,
) -> Result<(), String> {
    let template = output_folder.join("segment-%03d.mp3");

    // -nostdin: ffmpeg が stdin を対話的に読まないことを明示 (SIGTTIN ハング防止)。
    let mut args = vec![
        OsString::from("-y"),
        OsString::from("-nostdin"),
        OsString::from("-i"),
        input.as_os_str().into(),
    ];
    args.extend(base_audio_args(optimize_for_size));
    args.extend([
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        OsString::from("-f"),
        OsString::from("segment"),
        OsString::from("-segment_time"),
        OsString::from(segment_seconds.to_string()),
        OsString::from("-reset_timestamps"),
        OsString::from("1"),
        template.as_os_str().into(),
    ]);

    run_ffmpeg_with_cancel(
        progress,
        ffmpeg,
        args,
        duration_seconds,
        "変換中",
        "MP3を分割しながら生成しています。",
        cancel,
    )
}

/// **既に正規化済み・エンコード済みの MP3** を `-c copy` でセグメント分割する
/// (再エンコード無し)。Slice D の二重エンコード解消のためのコア関数。
///
/// ## なぜ別関数なのか (`run_split_conversion` との違い)
/// - [`run_split_conversion`] は **ローカル / 直リンク経路** 用で、入力が任意
///   フォーマット (正規化前) のため `base_audio_args`
///   (`libmp3lame -ar 44100 -ac 2 -map_metadata -1`) で **再エンコードしながら**
///   分割する。これは既存仕様 (Slice C ゴールデンが固定) であり変更しない。
/// - 本関数は **yt-dlp 経路** 用。yt-dlp が `--postprocessor-args`
///   (`ExtractAudio:-ar 44100 -ac 2 -map_metadata -1`) で既に 44100Hz / 2ch
///   正規化・メタ除去済みの MP3 を **単一エンコードパス** で生成しているため、
///   分割で再エンコードする必要が無い。ここでは `-c copy` (stream copy) で
///   セグメント化し、**2 回目の libmp3lame エンコードを避ける**。これにより
///   音質劣化と所要時間増を解消する。
///
/// `-c copy` セグメントは MP3 フレーム境界で切れるため秒境界はおおむね一致
/// (フレーム長 ~26ms の粒度) する。yt-dlp 経路は正規化を yt-dlp 側に委ねており、
/// ここでは「再エンコードしない」ことを優先する。
///
/// 入力 `input` は処理済み MP3 の絶対パス。`output_directory` 配下に
/// `<stem>-mp3-<timestamp>/segment-%03d.mp3` を生成し、[`ConversionSummary`] を返す
/// (`convert_media_blocking` の split 出力と同じ構造)。
pub fn segment_mp3_copy<R: BinaryResolver + ?Sized>(
    resolver: &R,
    input: &Path,
    output_directory: &Path,
    split_minutes: u64,
    progress: ProgressFn<'_>,
) -> Result<ConversionSummary, String> {
    // 後方互換: cancel トークン無し (None)。MCP / CLI 既存呼び出しは挙動不変。
    segment_mp3_copy_with_cancel(
        resolver,
        input,
        output_directory,
        split_minutes,
        progress,
        None,
    )
}

/// [`segment_mp3_copy`] の協調的キャンセル対応版 (Slice I-1 reviewer fix)。
///
/// `cancel` に `Some(flag)` を渡すと、内部の ffmpeg (`-c copy` セグメント) 行読み
/// ループ各反復でフラグを確認し、立っていれば ffmpeg child (プロセスグループ) を
/// kill して即座に中断する。`None` なら従来挙動と同一。`extract_url_to_mp3` の
/// 分割経路がこれを使う。
pub fn segment_mp3_copy_with_cancel<R: BinaryResolver + ?Sized>(
    resolver: &R,
    input: &Path,
    output_directory: &Path,
    split_minutes: u64,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
) -> Result<ConversionSummary, String> {
    // A14: 範囲検証 (1..=MAX_SPLIT_MINUTES)。秒変換は build_segment_copy_args 内で
    // saturating_mul を使う。
    let _ = split_minutes_to_seconds(split_minutes)?;
    if !input.exists() {
        return Err("入力ファイルが見つかりません。".into());
    }
    if !output_directory.exists() {
        return Err("保存先フォルダが見つかりません。".into());
    }

    progress(&Progress {
        percent: 0,
        step: "準備中".into(),
        detail: "分割エンジンを準備しています。".into(),
        current_seconds: 0.0,
        total_seconds: 0.0,
    })?;

    let ffmpeg = resolver.resolve("ffmpeg")?;
    let ffprobe = resolver.resolve("ffprobe")?;

    let duration_seconds = read_duration_seconds(&ffprobe, input)?;
    // create_output_folder は `<output_directory>/<stem>-mp3-<ts>/` という新規専用
    // ジョブフォルダを作って返す (output_directory 自体や他ファイルではない)。
    let output_folder = create_output_folder(input, output_directory)?;
    // A5/A6: ジョブフォルダ作成直後に RAII ガードを張る。ffmpeg 失敗 / cancel /
    // progress エラー、および summarize_outputs 失敗の全経路で Drop が専用ジョブ
    // フォルダを掃除し、partial な segment-*.mp3 / 空フォルダをユーザー出力先に
    // 残さない。output_directory 自体や他ファイルは削除対象に含めない。成功時のみ
    // disarm する。
    let mut output_guard = TempDirGuard::arm(output_folder.clone());

    let template = output_folder.join("segment-%03d.mp3");
    let args = build_segment_copy_args(input, split_minutes, &template);

    run_ffmpeg_with_cancel(
        progress,
        &ffmpeg,
        args,
        duration_seconds,
        "分割中",
        "MP3を再エンコードせずに分割しています。",
        cancel,
    )?;

    progress(&Progress {
        percent: 100,
        step: "完了".into(),
        detail: "分割が完了しました。".into(),
        current_seconds: duration_seconds,
        total_seconds: duration_seconds,
    })?;

    // summarize_outputs が成功して初めて成果物として確定する。失敗時は partial が
    // 残らないようガードを armed のままにし、成功時のみ disarm する。
    let summary = summarize_outputs(output_folder, duration_seconds)?;
    output_guard.disarm();
    Ok(summary)
}

/// `-c copy` セグメント分割の ffmpeg 引数を組み立てる (テスト可能なように分離)。
///
/// **`-c copy` (stream copy) のみを使い、`libmp3lame` での再エンコードや
/// `base_audio_args` (正規化引数) は一切含めない** ことが本関数の不変条件。
/// 正規化 (44100Hz/2ch/メタ除去) は yt-dlp の単一エンコードパス側で済んでいる
/// 前提で、ここでは「2 回目のエンコードをしない」ことだけを担保する。
///
/// `pub` 公開は、二重エンコード解消 (Slice D) の引数レベル検証
/// (`tests/split_no_reencode.rs`) が「split のために libmp3lame で再エンコード
/// していない (`-c copy` segment 方式である)」ことを deterministic に確認する
/// ため。
pub fn build_segment_copy_args(input: &Path, split_minutes: u64, template: &Path) -> Vec<OsString> {
    // A14: u64 オーバーフロー防止。範囲検証は呼び出し側で実施。
    let segment_seconds = split_minutes.saturating_mul(60);
    vec![
        OsString::from("-y"),
        // -nostdin: ffmpeg が stdin を対話的に読まないことを明示 (SIGTTIN ハング防止)。
        OsString::from("-nostdin"),
        OsString::from("-i"),
        input.as_os_str().into(),
        // 二重エンコード解消の要: 再エンコードせず stream copy する。
        OsString::from("-c"),
        OsString::from("copy"),
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        OsString::from("-f"),
        OsString::from("segment"),
        OsString::from("-segment_time"),
        OsString::from(segment_seconds.to_string()),
        OsString::from("-reset_timestamps"),
        OsString::from("1"),
        template.as_os_str().into(),
    ]
}

/// ffmpeg を起動し進捗をパースする (協調的キャンセル対応版・Slice I-1 reviewer fix)。
///
/// ## 指摘1: ffmpeg child のキャンセル kill 規律
/// 従来の `run_ffmpeg` は progress を `?` で返すだけで ffmpeg child を kill/wait して
/// いなかった。cancel が `convert_media` / Direct / HLS / `convert_url` 経路を通る今、
/// キャンセル時 (= progress が `Err` を返す) に ffmpeg child が orphan 化する。これを
/// `run_ytdlp_with_deadline` と**同等の kill 規律**で塞ぐ:
/// - (a) Unix で child を新プロセスグループのリーダー (`process_group(0)`) にする。
///   ffmpeg はフィルタ等で子孫を spawn し得るため、直下だけ kill すると子孫が
///   stdout/stderr FD を握り reader が EOF に到達しない。グループ分離で `killpg` の
///   グループ一括 SIGKILL を効かせる。
/// - (b) progress が `Err` を返した時 (= キャンセル / channel 切断)、および cancel
///   トークンが立った時に [`ytdlp::kill_and_reap`] (killpg + bounded reap) と
///   stdout/stderr reader の **bounded join** を行ってから中断エラーを返す。
/// - (c) 正常完了・通常エラー経路は従来どおり (余計な kill をしない)。
///
/// ## 指摘2: cancel トークンの即応チェック
/// 行読みループの各反復でトークンを確認し、立っていれば即 kill して中断する
/// (progress 通知を待たない)。
#[allow(clippy::too_many_arguments)]
fn run_ffmpeg_with_cancel(
    progress: ProgressFn<'_>,
    ffmpeg: &Path,
    args: Vec<OsString>,
    duration_seconds: f64,
    step: &str,
    detail: &str,
    cancel: Cancel<'_>,
) -> Result<(), String> {
    run_ffmpeg_with_cancel_inner(
        progress,
        ffmpeg,
        args,
        duration_seconds,
        step,
        detail,
        cancel,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_ffmpeg_with_cancel_with_deadline(
    progress: ProgressFn<'_>,
    ffmpeg: &Path,
    args: Vec<OsString>,
    duration_seconds: f64,
    step: &str,
    detail: &str,
    cancel: Cancel<'_>,
    deadline: Duration,
) -> Result<(), String> {
    run_ffmpeg_with_cancel_inner(
        progress,
        ffmpeg,
        args,
        duration_seconds,
        step,
        detail,
        cancel,
        Some(deadline),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_ffmpeg_with_cancel_inner(
    progress: ProgressFn<'_>,
    ffmpeg: &Path,
    args: Vec<OsString>,
    duration_seconds: f64,
    step: &str,
    detail: &str,
    cancel: Cancel<'_>,
    deadline: Option<Duration>,
) -> Result<(), String> {
    let started = Instant::now();
    let mut command = Command::new(ffmpeg);
    command
        .args(args)
        // SIGTTIN 根絶: process_group(0) で独立グループ化した ffmpeg が制御端末(tty)を
        // stdin から読もうとすると SIGTTIN で停止し、出力ゼロ・終了せずハングする
        // (GUI 起動時の「5% 変換中」ハングの根本原因)。stdin を /dev/null に差し替えて
        // 端末継承を断つ (引数側の `-nostdin` と二重の保険)。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 指摘1(a): ffmpeg child を新プロセスグループのリーダーにする (Unix)。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("ffmpeg の実行に失敗しました: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg の進捗出力を取得できませんでした。".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "ffmpeg のエラー出力を取得できませんでした。".to_string())?;

    let stderr_handle = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut reader = BufReader::new(stderr);
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;
        Ok(buffer)
    });

    // stdout は別スレッドで読み、行をチャネルへ送る。これにより行読みが無限ブロック
    // しても (cancel チェックや bounded join のために) main スレッドが制御を保てる。
    // yt-dlp 経路と同じ構造 (指摘1(b) の bounded join を成立させるため)。
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel::<String>();
    let stdout_handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if stdout_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut current_seconds = 0.0;
    let mut last_percent = 0;

    // 指摘1(b) ヘルパ: キャンセル / 中断時に child (グループ) を kill + reap し、
    // stdout/stderr reader を bounded join してから `err` を返す。orphan を残さない。
    let abort = |child: &mut std::process::Child,
                 stdout_handle: thread::JoinHandle<()>,
                 stderr_handle: thread::JoinHandle<std::io::Result<Vec<u8>>>,
                 err: String|
     -> Result<(), String> {
        ytdlp::kill_and_reap(child);
        ytdlp::bounded_join(stdout_handle);
        bounded_join_ffmpeg_stderr(stderr_handle);
        Err(err)
    };

    if let Err(error) = progress(&Progress {
        percent: 5,
        step: step.into(),
        detail: detail.into(),
        current_seconds: 0.0,
        total_seconds: duration_seconds,
    }) {
        return abort(&mut child, stdout_handle, stderr_handle, error);
    }

    // 行受信を FFMPEG_POLL_INTERVAL でタイムアウトさせ、毎周回で cancel トークンを
    // 確認する。これにより ffmpeg が無音 (進捗を出さない) でも ~100ms 以内で
    // キャンセルに反応できる (指摘2)。
    let status = loop {
        if deadline.is_some_and(|limit| started.elapsed() >= limit) {
            return abort(
                &mut child,
                stdout_handle,
                stderr_handle,
                "ffmpeg の処理が制限時間を超過しました。".to_string(),
            );
        }
        // 指摘2: 各周回でキャンセルトークンを確認 → 立っていれば即 kill して中断。
        if ytdlp::is_cancelled(cancel) {
            return abort(
                &mut child,
                stdout_handle,
                stderr_handle,
                CANCEL_MESSAGE.to_string(),
            );
        }

        match stdout_rx.recv_timeout(FFMPEG_POLL_INTERVAL) {
            Ok(line) => {
                // 指摘1(b): progress が Err (キャンセル / channel 切断) を返したら、単に
                // return せず child (グループ) を kill + reap してから中断する。
                if let Err(error) = handle_ffmpeg_line(
                    &line,
                    progress,
                    &mut current_seconds,
                    &mut last_percent,
                    duration_seconds,
                    step,
                    detail,
                ) {
                    return abort(&mut child, stdout_handle, stderr_handle, error);
                }
            }
            // この周回では新しい行が無い。プロセスが既に終了していれば抜ける。ただし
            // チャネルに残る行 (最後の `progress=end` 等) を落とさないよう、break 前に
            // 残行を **非ブロッキングで drain** して進捗通知を発火させる (try_recv が
            // 空 / 切断になるまで)。これにより yt-dlp 経路と同じく try_wait で確実に
            // 終了検知しつつ (FD を握る孫がいても再ハングしない)、最終進捗も取りこぼさない。
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    while let Ok(line) = stdout_rx.try_recv() {
                        if let Err(error) = handle_ffmpeg_line(
                            &line,
                            progress,
                            &mut current_seconds,
                            &mut last_percent,
                            duration_seconds,
                            step,
                            detail,
                        ) {
                            return abort(&mut child, stdout_handle, stderr_handle, error);
                        }
                    }
                    break status;
                }
            }
            // stdout スレッドが pipe 終了で送信を閉じた (全行 drain 済み)。終了を待つ。
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|error| format!("ffmpeg の終了待機に失敗しました: {error}"))?
                {
                    break status;
                }
                thread::sleep(FFMPEG_POLL_INTERVAL);
                continue;
            }
        }
    };

    // 正常経路: stdout reader を join し (pipe EOF 済みのはず)、stderr を回収する。
    ytdlp::bounded_join(stdout_handle);
    let stderr = stderr_handle
        .join()
        .map_err(|_| "ffmpeg のエラー出力スレッドが異常終了しました。".to_string())?
        .map_err(|error| format!("ffmpeg のエラー出力を読み取れませんでした: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(stderr_message("変換に失敗しました。", &stderr))
    }
}

/// ffmpeg の `-progress pipe:1` 出力 1 行を解釈し、必要なら進捗通知を発火する。
///
/// 行解析ロジック (out_time_us / out_time_ms / out_time / progress=continue|end) を
/// 1 か所に集約し、通常受信経路と「プロセス終了後の残行 drain」経路の双方から
/// 再利用する (進捗の取りこぼし防止)。`progress` が `Err` を返したらそのまま伝播
/// (呼び出し側が child kill + 中断を行う)。`current_seconds` / `last_percent` は
/// 行を跨いで更新するため `&mut` で受ける。
#[allow(clippy::too_many_arguments)]
fn handle_ffmpeg_line(
    line: &str,
    progress: ProgressFn<'_>,
    current_seconds: &mut f64,
    last_percent: &mut u8,
    duration_seconds: f64,
    step: &str,
    detail: &str,
) -> Result<(), String> {
    if let Some(value) = line.strip_prefix("out_time_us=") {
        if let Ok(parsed) = value.parse::<f64>() {
            *current_seconds = parsed / 1_000_000.0;
        }
    } else if let Some(value) = line.strip_prefix("out_time_ms=") {
        if let Ok(parsed) = value.parse::<f64>() {
            *current_seconds = parsed / 1_000.0;
        }
    } else if let Some(value) = line.strip_prefix("out_time=") {
        *current_seconds = parse_ffmpeg_time(value).unwrap_or(*current_seconds);
    } else if line == "progress=continue" || line == "progress=end" {
        let is_end = line == "progress=end";
        let percent = calculate_percent(*current_seconds, duration_seconds, is_end);
        if percent > *last_percent || is_end {
            *last_percent = percent;
            progress(&Progress {
                percent,
                step: if is_end {
                    "仕上げ中".into()
                } else {
                    step.into()
                },
                detail: detail.into(),
                current_seconds: current_seconds.min(duration_seconds),
                total_seconds: duration_seconds,
            })?;
        }
    }
    Ok(())
}

/// ffmpeg 進捗行受信のポーリング間隔 (指摘2)。yt-dlp 経路と同じ 100ms 粒度で
/// cancel トークンを確認できるようにする。
const FFMPEG_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// ffmpeg の stderr reader thread (`JoinHandle<io::Result<Vec<u8>>>`) を bounded に
/// join する (指摘1(b))。group kill 後はほぼ即 EOF で終了するため短い猶予でよい。
/// 猶予を超えたら join せず detach する (素の `join()` での再ハングを断つ)。
/// 中断経路でのみ使う (収集した stderr は中断エラーには使わないので破棄する)。
fn bounded_join_ffmpeg_stderr(handle: thread::JoinHandle<std::io::Result<Vec<u8>>>) {
    let started = Instant::now();
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        if started.elapsed() >= FFMPEG_READER_JOIN_TIMEOUT {
            return;
        }
        thread::sleep(FFMPEG_POLL_INTERVAL);
    }
}

/// ffmpeg reader thread の bounded join 上限 (指摘1(b))。
const FFMPEG_READER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

fn calculate_percent(current_seconds: f64, duration_seconds: f64, is_end: bool) -> u8 {
    if is_end {
        return 99;
    }

    if duration_seconds <= 0.0 {
        return 5;
    }

    let raw = ((current_seconds / duration_seconds) * 100.0).round() as i32;
    raw.clamp(5, 99) as u8
}

fn parse_ffmpeg_time(value: &str) -> Option<f64> {
    let mut parts = value.split(':');
    let hours = parts.next()?.parse::<f64>().ok()?;
    let minutes = parts.next()?.parse::<f64>().ok()?;
    let seconds = parts.next()?.parse::<f64>().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn summarize_outputs(
    output_folder: PathBuf,
    duration_seconds: f64,
) -> Result<ConversionSummary, String> {
    let mut files = Vec::new();

    for entry in fs::read_dir(&output_folder)
        .map_err(|error| format!("出力フォルダを読み取れませんでした: {error}"))?
    {
        let entry = entry.map_err(|error| format!("出力ファイルの取得に失敗しました: {error}"))?;
        let path = entry.path();

        if path.extension().and_then(|value| value.to_str()) != Some("mp3") {
            continue;
        }

        let metadata = fs::metadata(&path)
            .map_err(|error| format!("出力ファイルの情報を取得できませんでした: {error}"))?;

        files.push(FileSummary {
            path: path.to_string_lossy().to_string(),
            size_bytes: metadata.len(),
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));

    let total_size_bytes = files.iter().map(|file| file.size_bytes).sum();

    Ok(ConversionSummary {
        output_folder: output_folder.to_string_lossy().to_string(),
        file_count: files.len(),
        total_size_bytes,
        files,
        duration_seconds,
    })
}

fn stderr_message(prefix: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let trimmed = detail.trim();

    if trimmed.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix} {trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> Url {
        Url::parse(url).unwrap()
    }

    #[test]
    fn sanitize_name_replaces_reserved_chars() {
        assert_eq!(
            sanitize_name("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_name("normal name"), "normal name");
        assert_eq!(sanitize_name("日本語ファイル"), "日本語ファイル");
    }

    // A16: `..` / 制御文字 / 先頭末尾ドット / 超長名の正規化。
    #[test]
    fn sanitize_name_normalizes_traversal_control_and_length() {
        // `.` / `..` / 空白のみ → 既定名。
        assert_eq!(sanitize_name(".."), "audio");
        assert_eq!(sanitize_name("."), "audio");
        assert_eq!(sanitize_name("   "), "audio");
        // 先頭末尾ドットは畳まれる (隠しファイル化防止)。
        assert_eq!(sanitize_name("..hidden.."), "hidden");
        // 制御文字は `_` へ。
        assert_eq!(sanitize_name("a\tb\nc"), "a_b_c");
        // 255B を超える名前は 200B 以内へ切り詰め (char 境界維持)。
        let long = "あ".repeat(500); // 1 文字 3 byte
        let out = sanitize_name(&long);
        assert!(out.len() <= SANITIZE_NAME_MAX_BYTES, "len={}", out.len());
        assert!(!out.is_empty());
        // 切り詰めても valid UTF-8 (panic しない / 既に String なので保証)。
        assert!(out.chars().all(|c| c == 'あ'));
    }

    // A14: split_minutes の範囲検証と秒変換 (オーバーフロー無し)。
    #[test]
    fn split_minutes_to_seconds_validates_range() {
        assert_eq!(split_minutes_to_seconds(1), Ok(60));
        assert_eq!(
            split_minutes_to_seconds(MAX_SPLIT_MINUTES),
            Ok(MAX_SPLIT_MINUTES * 60)
        );
        assert!(split_minutes_to_seconds(0).is_err());
        assert!(split_minutes_to_seconds(MAX_SPLIT_MINUTES + 1).is_err());
        // u64::MAX でも panic / wrap せずエラー (範囲外)。
        assert!(split_minutes_to_seconds(u64::MAX).is_err());
    }

    // A18: 認証マーカーの付与判定 / 除去。
    #[test]
    fn auth_required_marker_roundtrip() {
        let raw = "サーバーがエラーを返しました: HTTP 403";
        let marked = format!("{AUTH_REQUIRED_MARKER}{raw}");
        assert!(is_auth_required_error(&marked));
        assert!(!is_auth_required_error(raw));
        assert_eq!(strip_auth_required_marker(&marked), raw);
        // マーカー無しはそのまま返す。
        assert_eq!(strip_auth_required_marker(raw), raw);
    }

    // A18: ffmpeg stderr の HTTP auth 失敗検出。
    #[test]
    fn looks_like_http_auth_failure_detects_401_403() {
        assert!(looks_like_http_auth_failure(
            "Server returned 401 Unauthorized"
        ));
        assert!(looks_like_http_auth_failure("HTTP error 403 Forbidden"));
        assert!(looks_like_http_auth_failure("Forbidden"));
        assert!(!looks_like_http_auth_failure("Connection timed out"));
        assert!(!looks_like_http_auth_failure("404 Not Found"));
    }

    // A6: 専用一時ディレクトリ配下のファイル削除で空ディレクトリも掃除する。
    // revmagic-downloads 配下でない親は触らない。
    #[test]
    fn cleanup_download_temp_removes_file_and_dedicated_dir() {
        let base = std::env::temp_dir().join(format!(
            "revmagic-cleanup-test-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        let downloads = base.join("revmagic-downloads");
        let job = downloads.join("20260625-000000-1");
        fs::create_dir_all(&job).unwrap();
        let file = job.join("clip.mp4");
        fs::write(&file, b"data").unwrap();

        cleanup_download_temp(&file.to_string_lossy());
        assert!(!file.exists(), "temp file must be removed");
        assert!(!job.exists(), "empty dedicated temp dir must be removed");

        // 非 revmagic-downloads 配下の親は消さない (利用者フォルダ保護)。
        let user_dir = base.join("my-output");
        fs::create_dir_all(&user_dir).unwrap();
        let user_file = user_dir.join("clip.mp4");
        fs::write(&user_file, b"data").unwrap();
        cleanup_download_temp(&user_file.to_string_lossy());
        assert!(!user_file.exists(), "file is still removed");
        assert!(
            user_dir.exists(),
            "non-dedicated parent dir must be preserved"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn infer_extension_prefers_path_extension() {
        assert_eq!(
            infer_extension(&parse("https://e.com/clip.mp4"), None).as_deref(),
            Some("mp4")
        );
        assert_eq!(
            infer_extension(&parse("https://e.com/audio.m4a?token=1"), None).as_deref(),
            Some("m4a")
        );
    }

    #[test]
    fn infer_extension_falls_back_to_content_type() {
        assert_eq!(
            infer_extension(&parse("https://e.com/stream"), Some("video/mp4")).as_deref(),
            Some("mp4")
        );
        assert_eq!(
            infer_extension(&parse("https://e.com/stream"), Some("audio/mpeg")).as_deref(),
            Some("mp3")
        );
        assert_eq!(
            infer_extension(&parse("https://e.com/stream"), Some("audio/x-m4a")).as_deref(),
            Some("m4a")
        );
    }

    #[test]
    fn infer_extension_rejects_unknown() {
        assert_eq!(
            infer_extension(&parse("https://e.com/file.txt"), Some("text/plain")),
            None
        );
        assert_eq!(infer_extension(&parse("https://e.com/file"), None), None);
    }

    #[test]
    fn extract_url_stem_returns_basename_without_extension() {
        assert_eq!(
            extract_url_stem(&parse("https://e.com/path/clip.mp4")),
            "clip"
        );
        assert_eq!(extract_url_stem(&parse("https://e.com/")), "download");
        assert_eq!(
            extract_url_stem(&parse("https://e.com/a/b/movie.mov?x=1")),
            "movie"
        );
    }

    #[test]
    fn is_hls_url_detects_m3u8() {
        assert!(is_hls_url(&parse("https://e.com/master.m3u8")));
        assert!(is_hls_url(&parse("https://e.com/master.m3u8?token=1")));
        assert!(is_hls_url(&parse("https://e.com/v.m3u8/seg.ts")));
        assert!(!is_hls_url(&parse("https://e.com/clip.mp4")));
    }

    #[test]
    fn build_headers_includes_ua_and_optional_fields() {
        let headers = build_ffmpeg_http_headers(Some("a=b"), Some("https://ref/"));
        assert!(headers.contains("User-Agent: Mozilla/5.0"));
        assert!(headers.contains("Referer: https://ref/\r\n"));
        assert!(headers.contains("Cookie: a=b\r\n"));

        let bare = build_ffmpeg_http_headers(None, None);
        assert!(bare.contains("User-Agent:"));
        assert!(!bare.contains("Referer:"));
        assert!(!bare.contains("Cookie:"));

        // 空文字 / 空白は無視される
        let empty = build_ffmpeg_http_headers(Some("  "), Some(""));
        assert!(!empty.contains("Cookie:"));
        assert!(!empty.contains("Referer:"));
    }

    #[test]
    fn base_audio_args_uses_bounded_transcription_profile_when_optimized() {
        let normal = base_audio_args(false);
        let small = base_audio_args(true);
        let q_normal = normal
            .iter()
            .position(|a| a == "-q:a")
            .map(|i| &normal[i + 1]);
        let bitrate = small
            .iter()
            .position(|a| a == "-b:a")
            .map(|i| &small[i + 1]);
        assert_eq!(q_normal.map(|s| s.to_str().unwrap()), Some("2"));
        assert_eq!(bitrate.map(|s| s.to_str().unwrap()), Some("64k"));
        assert!(small.iter().any(|a| a == "24000"));
        assert!(normal.iter().any(|a| a == "libmp3lame"));
        assert!(normal.iter().any(|a| a == "44100"));
    }

    // SIGTTIN ハング回帰防止: progress pipe を使う全 ffmpeg 引数経路に `-nostdin` が
    // 含まれること。process_group(0) で独立グループ化した ffmpeg が端末 stdin を
    // 読みに行って停止する事故 (GUI「5% 変換中」ハング) を引数レベルでも封じる。
    #[test]
    fn ffmpeg_arg_builders_include_nostdin() {
        let input = Path::new("/tmp/in.m4a");
        let output = Path::new("/tmp/out.mp3");
        let template = Path::new("/tmp/segment-%03d.mp3");

        let has_nostdin = |args: &[OsString]| args.iter().any(|a| a == "-nostdin");

        assert!(
            has_nostdin(&build_single_copy_args(input, output)),
            "single_copy missing -nostdin"
        );
        assert!(
            has_nostdin(&build_single_reencode_args(input, output, false)),
            "single_reencode missing -nostdin"
        );
        assert!(
            has_nostdin(&build_single_reencode_args(input, output, true)),
            "single_reencode(optimize) missing -nostdin"
        );
        assert!(
            has_nostdin(&build_segment_copy_args(input, 10, template)),
            "segment_copy missing -nostdin"
        );
    }

    // Speed-1: 並列分割のセグメント範囲計算 (連続性 / 抜け重複なし / 個数)。
    #[test]
    fn compute_segment_ranges_is_continuous_and_covers_full_length() {
        // 100 秒を 30 秒粒度 → 4 セグメント (30,30,30,10)、全長カバー。
        let ranges = compute_segment_ranges(100.0, 30);
        assert_eq!(
            ranges.len(),
            4,
            "ceil(100/30)=4 セグメントになっていない: {ranges:?}"
        );
        // index は 0..n の連番。
        for (i, r) in ranges.iter().enumerate() {
            assert_eq!(r.index, i, "index 連番でない: {ranges:?}");
        }
        // 連続性: 各 start は前 (start+len) と一致 (抜け / 重複なし)。
        for w in ranges.windows(2) {
            let prev_end = w[0].start + w[0].len;
            assert!(
                (prev_end - w[1].start).abs() < 1e-9,
                "セグメント境界に隙間 / 重複: prev_end={prev_end} next_start={}",
                w[1].start
            );
        }
        // 先頭は 0 から、末尾は全長 (100) で終わる (全長カバー)。
        assert!((ranges.first().unwrap().start).abs() < 1e-9);
        let last = ranges.last().unwrap();
        assert!(
            ((last.start + last.len) - 100.0).abs() < 1e-9,
            "末尾が全長をカバーしていない: {last:?}"
        );
        // 末尾 len は残り 10 秒。
        assert!(
            (last.len - 10.0).abs() < 1e-9,
            "末尾 len が残り時間でない: {last:?}"
        );
    }

    #[test]
    fn compute_segment_ranges_exact_multiple_has_no_extra_segment() {
        // 90 秒を 30 秒粒度 → ちょうど 3 セグメント (端数で 4 個目を作らない)。
        let ranges = compute_segment_ranges(90.0, 30);
        assert_eq!(
            ranges.len(),
            3,
            "割り切れる場合に余分なセグメントが出ている: {ranges:?}"
        );
        for r in &ranges {
            assert!((r.len - 30.0).abs() < 1e-9, "各 len が 30 秒でない: {r:?}");
        }
        let last = ranges.last().unwrap();
        assert!(((last.start + last.len) - 90.0).abs() < 1e-9);
    }

    #[test]
    fn compute_segment_ranges_short_or_unknown_duration_is_single_segment() {
        // duration <= segment は 1 セグメント (全体)。
        assert_eq!(compute_segment_ranges(20.0, 30).len(), 1);
        // duration 不明 (0.0) / 非正 / segment 0 は 1 セグメント (全体)。
        assert_eq!(compute_segment_ranges(0.0, 30).len(), 1);
        assert_eq!(compute_segment_ranges(-5.0, 30).len(), 1);
        assert_eq!(compute_segment_ranges(100.0, 0).len(), 1);
        assert_eq!(compute_segment_ranges(f64::NAN, 30).len(), 1);
    }

    // Speed-1: 1 セグメント分の独立 ffmpeg 引数 (-ss/-t / 再エンコード / -nostdin)。
    #[test]
    fn build_segment_reencode_args_wires_seek_duration_and_encode() {
        let input = Path::new("/tmp/in.mp4");
        let out = Path::new("/tmp/out/segment-002.mp3");
        let range = SegmentRange {
            index: 2,
            start: 60.0,
            len: 30.0,
        };
        let args = build_segment_reencode_args(input, range, out, false);
        let as_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let window_has =
            |flag: &str, val: &str| as_str.windows(2).any(|w| w[0] == flag && w[1] == val);
        // -ss は -i より前 (input seek)。
        let ss_pos = as_str.iter().position(|a| a == "-ss").expect("-ss なし");
        let i_pos = as_str.iter().position(|a| a == "-i").expect("-i なし");
        assert!(
            ss_pos < i_pos,
            "-ss が -i より前にない (input seek でない): {as_str:?}"
        );
        assert!(
            window_has("-ss", "60.000000"),
            "start 秒が渡っていない: {as_str:?}"
        );
        assert!(
            window_has("-t", "30.000000"),
            "len 秒が渡っていない: {as_str:?}"
        );
        assert!(
            window_has("-i", "/tmp/in.mp4"),
            "入力が -i で渡っていない: {as_str:?}"
        );
        // 再エンコード (各セグメント並列再エンコードで多コア活用)。
        assert!(
            as_str.iter().any(|a| a == "libmp3lame"),
            "再エンコードでない: {as_str:?}"
        );
        // SIGTTIN 回避。
        assert!(
            as_str.iter().any(|a| a == "-nostdin"),
            "-nostdin なし: {as_str:?}"
        );
        // 出力は末尾。
        assert_eq!(
            as_str.last().map(String::as_str),
            Some("/tmp/out/segment-002.mp3")
        );
    }

    #[test]
    fn calculate_percent_clamps_and_handles_end() {
        assert_eq!(calculate_percent(0.0, 0.0, false), 5);
        assert_eq!(calculate_percent(50.0, 100.0, false), 50);
        assert_eq!(calculate_percent(99.0, 100.0, false), 99);
        assert_eq!(calculate_percent(200.0, 100.0, false), 99);
        assert_eq!(calculate_percent(0.1, 100.0, false), 5);
        assert_eq!(calculate_percent(50.0, 100.0, true), 99);
    }

    #[test]
    fn parse_ffmpeg_time_parses_hms() {
        assert_eq!(parse_ffmpeg_time("00:00:10.000000"), Some(10.0));
        assert_eq!(parse_ffmpeg_time("01:02:03.000000"), Some(3723.0));
        assert_eq!(parse_ffmpeg_time("garbage"), None);
    }

    #[test]
    fn format_bytes_uses_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn fallback_resolver_finds_ffmpeg_on_path() {
        // CI / dev 環境では ffmpeg が PATH / Homebrew にある前提のテスト。
        // 無い環境では skip 相当 (panic させない)。
        let resolver = FallbackBinaryResolver::new();
        if let Ok(path) = resolver.resolve("ffmpeg") {
            assert!(path.exists());
        }
    }

    /// F2: 0-byte placeholder を skip し、後続候補の実バイナリへ fallthrough する。
    #[cfg(unix)]
    #[test]
    fn fallback_resolver_skips_zero_byte_placeholder() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!(
            "revmagic-resolver-test-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        // resource_dir A: build.rs の placeholder 相当 (0-byte / 0755)。
        let placeholder_dir = base.join("a");
        let placeholder_bin = placeholder_dir.join("binaries");
        fs::create_dir_all(&placeholder_bin).unwrap();
        let placeholder = placeholder_bin.join("yt-dlp");
        fs::write(&placeholder, b"").unwrap();
        fs::set_permissions(&placeholder, fs::Permissions::from_mode(0o755)).unwrap();

        // resource_dir B: self-contained な実バイナリ相当 (非0byte / 0755 / 非 shebang)。
        // F1: yt-dlp は極小 shebang shim を弾くため、実体相当は閾値超の binary 風
        // バイト列とする。
        let real_dir = base.join("b");
        let real_bin = real_dir.join("binaries");
        fs::create_dir_all(&real_bin).unwrap();
        let real = real_bin.join("yt-dlp");
        fs::write(&real, vec![0x7fu8; (YTDLP_SHIM_MAX_BYTES as usize) + 1]).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

        // placeholder を先に積む -> 0-byte なので skip され、実体 B が採用される。
        let resolved =
            fallback_resolve_binary("yt-dlp", &[placeholder_dir.clone(), real_dir.clone()])
                .expect("resolver should fall through to the real binary");
        assert_eq!(
            resolved, real,
            "placeholder must be skipped, real binary chosen"
        );
        assert_ne!(
            resolved, placeholder,
            "must not pick the 0-byte placeholder"
        );

        // 直接 helper も検証: 0-byte は不可、実体は可。
        assert!(
            !is_usable_binary(&placeholder),
            "0-byte placeholder must be unusable"
        );
        assert!(
            is_usable_binary(&real),
            "non-zero 0755 binary must be usable"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// テスト用のスクリプトベース resolver。`ffmpeg` / `ffprobe` を任意の
    /// shell スクリプトに差し替えて、失敗 ffmpeg / 成功 ffprobe を deterministic に
    /// 再現する。bin ディレクトリを温存し、テスト終了時に呼び出し側が掃除する。
    #[cfg(unix)]
    struct ScriptResolver {
        ffmpeg: PathBuf,
        ffprobe: PathBuf,
    }

    #[cfg(unix)]
    impl ScriptResolver {
        fn new(base: &Path, ffmpeg_script: &str, ffprobe_script: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            fs::create_dir_all(base).unwrap();
            let ffmpeg = base.join("ffmpeg");
            let ffprobe = base.join("ffprobe");
            fs::write(&ffmpeg, ffmpeg_script).unwrap();
            fs::write(&ffprobe, ffprobe_script).unwrap();
            fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&ffprobe, fs::Permissions::from_mode(0o755)).unwrap();
            Self { ffmpeg, ffprobe }
        }
    }

    #[cfg(unix)]
    impl BinaryResolver for ScriptResolver {
        fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
            match binary {
                "ffmpeg" => Ok(self.ffmpeg.clone()),
                "ffprobe" => Ok(self.ffprobe.clone()),
                other => Err(format!("unsupported binary: {other}")),
            }
        }
    }

    #[cfg(unix)]
    fn noop_progress() -> impl Fn(&Progress) -> Result<(), String> + Send + Sync {
        |_: &Progress| Ok(())
    }

    /// 共有 `$TMPDIR/revmagic-downloads/` を観測 / 書き込みする HLS テスト同士を直列化
    /// するためのロック。これらのテストはグローバルな temp ディレクトリを介して相互に
    /// 干渉し得る (before/after スナップショット差分が他テストの一時ディレクトリを拾う)
    /// ため、cargo の並列実行下で deterministic にするには相互排他が要る。
    #[cfg(unix)]
    static SHARED_DOWNLOADS_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// FIX-1 回帰: HLS ダウンロードの ffmpeg が失敗した場合、作成した専用 temp_dir
    /// (revmagic-downloads/<ts>/) と部分 .mp4 が残らないこと (RAII ガード掃除)。
    #[cfg(unix)]
    #[test]
    fn hls_download_ffmpeg_failure_cleans_temp_dir() {
        // 共有 revmagic-downloads を観測するため、同ディレクトリを使う他テストと直列化。
        let _serial = SHARED_DOWNLOADS_DIR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = std::env::temp_dir().join(format!(
            "revmagic-hls-fail-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        // ffmpeg: 出力先 (末尾引数) に部分ファイルを書いてから失敗する。
        let resolver = ScriptResolver::new(
            &base,
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\n: > \"$last\" 2>/dev/null\nexit 1\n",
            "#!/bin/sh\nexit 1\n",
        );

        let downloads = std::env::temp_dir().join("revmagic-downloads");
        let before = snapshot_dir_names(&downloads);

        let progress = noop_progress();
        let result = download_hls_blocking_with_cancel(
            &resolver,
            parse("https://example.com/master.m3u8"),
            None,
            None,
            &progress,
            None,
        );
        assert!(result.is_err(), "failing ffmpeg must return Err");

        let after = snapshot_dir_names(&downloads);
        let leaked: Vec<_> = after.difference(&before).collect();
        assert!(
            leaked.is_empty(),
            "no new temp dir must remain after ffmpeg failure, leaked: {leaked:?}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// FIX-1 回帰 (★秘密漏洩): HLS の ffmpeg が署名 URL (`?token=...`) を含む stderr を
    /// 出して 403 で失敗した場合に、返るエラー文言へ query 生値 (token/sig) が漏れない
    /// こと (redact 済み) かつ AUTH_REQUIRED_MARKER は付与され is_auth_required_error が
    /// true のままであることを検証する。direct 経路の `direct_download_error_redacts_url_query`
    /// に対応する HLS 版回帰。
    #[cfg(unix)]
    #[test]
    fn hls_download_error_redacts_url_query_and_keeps_auth_marker() {
        // 共有 revmagic-downloads を介して上のスナップショットテストと干渉しないよう直列化。
        let _serial = SHARED_DOWNLOADS_DIR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = std::env::temp_dir().join(format!(
            "revmagic-hls-redact-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        // ffmpeg: 実 ffmpeg が 403 失敗時に入力 URL を stderr に出す挙動を模す。末尾引数
        // (出力先) は触らず、stderr に署名 URL を含む 403 文言を出して exit 1。
        // ffprobe: HLS 経路では duration 失敗は 0.0 に握り潰されるため exit 1 でよい。
        let resolver = ScriptResolver::new(
      &base,
      "#!/bin/sh\n>&2 echo \"[https @ 0x0] HTTP error 403 Forbidden\"\n>&2 echo \"https://cdn.example.com/master.m3u8?token=SECRETSIG&sig=DEADBEEF: Server returned 403 Forbidden\"\nexit 1\n",
      "#!/bin/sh\nexit 1\n",
    );

        let progress = noop_progress();
        let result = download_hls_blocking_with_cancel(
            &resolver,
            parse("https://cdn.example.com/master.m3u8?token=SECRETSIG&sig=DEADBEEF"),
            None,
            None,
            &progress,
            None,
        );

        let err = result.expect_err("failing ffmpeg (403) must return Err");
        // query トークン生値が漏れていないこと (redact 済み)。
        assert!(!err.contains("SECRETSIG"), "url query token leaked: {err}");
        assert!(!err.contains("DEADBEEF"), "url sig leaked: {err}");
        assert!(!err.contains("token="), "url query key leaked: {err}");
        // 認証マーカーは付与され、種別判定は true のまま (フロントの authRequired 合流)。
        assert!(
            is_auth_required_error(&err),
            "403 HLS error must carry auth marker: {err}"
        );
        // 表示用文言にも秘密が無いこと (marker 除去後)。
        let display = strip_auth_required_marker(&err);
        assert!(
            !display.contains("SECRETSIG"),
            "stripped token leaked: {display}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// FIX-2 回帰: direct download の cancel 経路で作成した専用 temp_dir と部分
    /// ファイルが残らないこと。実ネットワークを使わず、`download_media_with_cancel`
    /// に到達できる範囲で間接検証するのは困難なため、本テストは TempDirGuard 単体の
    /// disarm / drop 掃除契約を検証する (FIX-2 の掃除はこのガードに一本化されている)。
    #[cfg(unix)]
    #[test]
    fn temp_dir_guard_removes_on_drop_and_preserves_on_disarm() {
        let base = std::env::temp_dir().join(format!(
            "revmagic-guard-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));

        // drop (armed) は dir を消す。
        let armed_dir = base.join("armed");
        fs::create_dir_all(&armed_dir).unwrap();
        fs::write(armed_dir.join("partial.mp4"), b"partial").unwrap();
        {
            let _guard = TempDirGuard::arm(armed_dir.clone());
        }
        assert!(!armed_dir.exists(), "armed guard must remove dir on drop");

        // disarm 後の drop は dir を残す (成功パスの成果物保護)。
        let kept_dir = base.join("kept");
        fs::create_dir_all(&kept_dir).unwrap();
        fs::write(kept_dir.join("result.mp4"), b"ok").unwrap();
        {
            let mut guard = TempDirGuard::arm(kept_dir.clone());
            guard.disarm();
        }
        assert!(
            kept_dir.exists(),
            "disarmed guard must preserve dir on drop"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// FIX-3 回帰: segment 分割の ffmpeg が失敗した場合、作成したユーザー出力先の
    /// 専用ジョブフォルダ (<stem>-mp3-<ts>/) と partial segment が残らず、かつ
    /// output_directory 自体や他ファイルは消えないこと。
    #[cfg(unix)]
    #[test]
    fn segment_copy_ffmpeg_failure_cleans_job_folder_only() {
        let base = std::env::temp_dir().join(format!(
            "revmagic-seg-fail-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        fs::create_dir_all(&base).unwrap();

        // 入力ファイル (存在チェックを通すだけ)。
        let input = base.join("clip.mp3");
        fs::write(&input, b"audio").unwrap();

        // ユーザー出力ディレクトリと、その中の無関係な既存ファイル (保護対象)。
        let output_dir = base.join("output");
        fs::create_dir_all(&output_dir).unwrap();
        let bystander = output_dir.join("keep-me.txt");
        fs::write(&bystander, b"do not delete").unwrap();

        // ffprobe は長さを返して成功、ffmpeg は partial segment を書いてから失敗。
        let bin = base.join("bin");
        let resolver = ScriptResolver::new(
      &bin,
      "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ndir=$(dirname \"$last\")\n: > \"$dir/segment-000.mp3\" 2>/dev/null\nexit 1\n",
      "#!/bin/sh\necho 120.0\n",
    );

        let progress = noop_progress();
        let result =
            segment_mp3_copy_with_cancel(&resolver, &input, &output_dir, 10, &progress, None);
        assert!(result.is_err(), "failing ffmpeg must return Err");

        // 専用ジョブフォルダ (<stem>-mp3-*) は残っていてはならない。
        let leaked_jobs: Vec<_> = fs::read_dir(&output_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains("-mp3-"))
            .collect();
        assert!(
            leaked_jobs.is_empty(),
            "partial job folder must be removed, leaked: {leaked_jobs:?}"
        );

        // output_directory 自体と無関係ファイルは保護される。
        assert!(
            output_dir.exists(),
            "user output directory must be preserved"
        );
        assert!(bystander.exists(), "unrelated file must be preserved");

        let _ = fs::remove_dir_all(&base);
    }

    /// FIX-4 回帰: ローカルファイル→MP3 変換 (convert_media_blocking_with_cancel)
    /// の ffmpeg が失敗した場合、create_output_folder が作ったユーザー出力先の専用
    /// ジョブフォルダ (<stem>-mp3-<ts>/) と partial 出力が残らず、かつ
    /// output_directory 自体や無関係ファイルは消えないこと。
    #[cfg(unix)]
    #[test]
    fn convert_media_ffmpeg_failure_cleans_job_folder_only() {
        let base = std::env::temp_dir().join(format!(
            "revmagic-conv-fail-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        fs::create_dir_all(&base).unwrap();

        // 入力ファイル (存在チェックを通すだけ)。
        let input = base.join("lecture.mp3");
        fs::write(&input, b"audio").unwrap();

        // ユーザー出力ディレクトリと、その中の無関係な既存ファイル (保護対象)。
        let output_dir = base.join("output");
        fs::create_dir_all(&output_dir).unwrap();
        let bystander = output_dir.join("keep-me.txt");
        fs::write(&bystander, b"do not delete").unwrap();

        // ffprobe は長さを返して成功、ffmpeg は partial 出力を書いてから失敗。
        // 末尾引数 (出力先パス) のディレクトリに部分ファイルを置いてから exit 1。
        let bin = base.join("bin");
        let resolver = ScriptResolver::new(
      &bin,
      "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ndir=$(dirname \"$last\")\n: > \"$dir/partial.mp3\" 2>/dev/null\nexit 1\n",
      "#!/bin/sh\necho 120.0\n",
    );

        let request = ConversionRequest {
            input_path: input.to_string_lossy().to_string(),
            output_directory: output_dir.to_string_lossy().to_string(),
            split_minutes: None,
            optimize_for_size: false,
        };

        let progress = noop_progress();
        let result = convert_media_blocking_with_cancel(&resolver, request, &progress, None);
        assert!(result.is_err(), "failing ffmpeg must return Err");

        // 専用ジョブフォルダ (<stem>-mp3-*) は残っていてはならない。
        let leaked_jobs: Vec<_> = fs::read_dir(&output_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains("-mp3-"))
            .collect();
        assert!(
            leaked_jobs.is_empty(),
            "partial job folder must be removed, leaked: {leaked_jobs:?}"
        );

        // output_directory 自体と無関係ファイルは保護される。
        assert!(
            output_dir.exists(),
            "user output directory must be preserved"
        );
        assert!(bystander.exists(), "unrelated file must be preserved");

        let _ = fs::remove_dir_all(&base);
    }

    /// 退行回帰: 秒粒度タイムスタンプ衝突下でも create_output_folder が必ず
    /// 新規・排他フォルダを返すこと。同一 stem・同一 output_root で連続2回呼び、
    /// (a) 異なるパスを返す、(b) 1回目フォルダに置いた目印ファイルが2回目呼び出し
    /// 後も残存する、ことを検証する。`create_dir_all` 退行だと2回目が1回目を再利用し、
    /// アーム済みガードの remove_dir_all が先行成果物を消し去る (データ消失) ため、
    /// この検証はその穴を直接塞ぐ。
    #[test]
    fn create_output_folder_collision_returns_fresh_exclusive_folder() {
        let base = std::env::temp_dir().join(format!(
            "revmagic-outfolder-collide-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        fs::create_dir_all(&base).unwrap();

        // 同一 stem・同一 output_root。秒粒度タイムスタンプを共有させるため連続呼び出し。
        let input = base.join("lecture.mp3");

        let first = create_output_folder(&input, &base).unwrap();
        // 1回目フォルダに先行成果物相当の目印を置く。
        let marker = first.join("first-job-output.mp3");
        fs::write(&marker, b"keep this").unwrap();

        let second = create_output_folder(&input, &base).unwrap();

        // (a) 2回目は1回目を再利用しない (異なるパス)。
        assert_ne!(
            first, second,
            "second call must return a distinct folder, got reuse: {first:?}"
        );
        // 2回目は新規・空であること。
        assert!(second.is_dir(), "second folder must exist");
        assert_eq!(
            fs::read_dir(&second).unwrap().count(),
            0,
            "second folder must be empty (freshly created)"
        );

        // (b) 1回目の目印ファイルは2回目呼び出し後も残存 (誤再利用ゼロの実証)。
        assert!(
            marker.exists(),
            "first job's output must survive the second call (no false reuse)"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// 決定論的退行回帰: `create_exclusive_dir` は candidate 同名が既存でも、
    /// **既存を再利用せず**サフィックス付き新規フォルダを排他作成し、既存フォルダ内の
    /// 目印ファイルを温存する。秒粒度衝突を実時間に依存させず、candidate を事前作成して
    /// `fs::create_dir` の `AlreadyExists` → サフィックスリトライ経路を確実に踏ませる。
    /// `create_dir_all` 退行だと既存フォルダを再利用し、アーム済みガードが先行成果物を
    /// 消し去る (データ消失) ため、この検証はその穴を直接塞ぐ。
    #[test]
    fn create_exclusive_dir_skips_preexisting_candidate_and_preserves_contents() {
        let base = std::env::temp_dir().join(format!(
            "revmagic-exclusive-dir-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        fs::create_dir_all(&base).unwrap();

        let base_name = "job-mp3-fixed";

        // candidate 同名 (suffix なし) を事前作成し、先行ジョブ成果物相当の目印を置く。
        let preexisting = base.join(base_name);
        fs::create_dir_all(&preexisting).unwrap();
        let marker = preexisting.join("first-job-output.mp3");
        fs::write(&marker, b"keep this").unwrap();

        // AlreadyExists → サフィックスリトライで別名の新規・排他フォルダを返す。
        let created = create_exclusive_dir(&base, base_name).unwrap();

        // (a) 既存 candidate を再利用しない。
        assert_ne!(
            created, preexisting,
            "must not reuse the pre-existing candidate folder"
        );
        // 新規・空であること。
        assert!(created.is_dir(), "returned folder must exist");
        assert_eq!(
            fs::read_dir(&created).unwrap().count(),
            0,
            "returned folder must be empty (freshly created)"
        );
        // 命名規約どおりサフィックス付き (base_name-2 等) であること。
        let created_name = created.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            created_name.starts_with(&format!("{base_name}-")),
            "returned folder name must carry a suffix, got {created_name:?}"
        );

        // (b) 既存フォルダの目印ファイルは温存される (誤再利用ゼロの実証)。
        assert!(
            marker.exists(),
            "pre-existing folder's content must survive (no false reuse)"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// `create_exclusive_dir` は親 (parent) が不在でも親のみを用意し、candidate を
    /// `fs::create_dir` で排他作成して新規・空フォルダを返す (NotFound 経路)。
    #[test]
    fn create_exclusive_dir_prepares_missing_parent() {
        let root = std::env::temp_dir().join(format!(
            "revmagic-exclusive-parent-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        // root 自体は存在しない (NotFound 経路を踏ませる)。親を事前に作らない。
        let parent = root.join("nested").join("downloads");

        let created = create_exclusive_dir(&parent, "job-mp3-fixed").unwrap();
        assert!(created.is_dir(), "returned folder must exist");
        assert!(
            created.starts_with(&parent),
            "returned folder must live under the prepared parent"
        );
        assert_eq!(
            fs::read_dir(&created).unwrap().count(),
            0,
            "returned folder must be empty (freshly created)"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// 指定ディレクトリの直下エントリ名集合を取る (存在しなければ空集合)。
    #[cfg(unix)]
    fn snapshot_dir_names(dir: &Path) -> std::collections::HashSet<String> {
        match fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(_) => std::collections::HashSet::new(),
        }
    }

    /// F2 回帰防止: 同梱の実 ffmpeg/ffprobe (非0byte/0755) が引き続き解決されること。
    #[cfg(unix)]
    #[test]
    fn fallback_resolver_still_resolves_real_bundled_binary() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!(
            "revmagic-resolver-ffmpeg-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        let bin = base.join("binaries");
        fs::create_dir_all(&bin).unwrap();
        // 実 ffmpeg/ffprobe を模した非0byte/0755 ファイル。
        for name in ["ffmpeg", "ffprobe"] {
            let path = bin.join(name);
            fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            let resolved = fallback_resolve_binary(name, std::slice::from_ref(&base))
                .unwrap_or_else(|e| panic!("bundled {name} should resolve: {e}"));
            assert_eq!(resolved, path, "bundled {name} must be picked");
        }
        let _ = fs::remove_dir_all(&base);
    }
}
