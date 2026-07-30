//! yt-dlp 統合 (講座系 / 埋め込み動画の解決)。
//!
//! 直リンクや素の HLS は [`crate::download_media`] が ffmpeg で処理できるが、
//! Loom 埋め込み (Utage 等の講座系) はページから本体 URL を解決できないため
//! 取得不可。実績上、`yt-dlp` に `--cookies` / `--referer` を付けて Loom を
//! 解決するのが唯一実用的なので、その経路を core に集約する。
//!
//! このモジュールは `desktop-host` に依存しない。cookie / referer / 進捗ブリッジは
//! 呼び出し側 (desktop host / CLI / MCP) が解決済みの値・コールバックで渡す。
//!
//! ## 秘密情報の取り扱い
//! - cookie 値は **argv に載せない**。`--cookies <netscape-file>` のファイル
//!   経由でのみ yt-dlp へ渡す。cookie file は 0600 の一時ファイルで、処理完了
//!   後 (成功 / 失敗いずれも) に必ず削除する。パスや中身を stdout / ログ /
//!   evidence へ生で出さない。
//! - referer / User-Agent は argv に載るが秘密ではない。ただしログ出力時は
//!   呼び出し側で redact する方針 (core はここでログを出さない)。

use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use chrono::Local;
use url::Url;

use crate::{
    create_exclusive_dir, sanitize_name, BinaryResolver, Cancel, ConversionSummary, FileSummary,
    Progress, ProgressFn,
};

/// 既定の User-Agent (実績スクリプトと同等の Safari UA)。
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) \
   AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";

/// `-f` に渡す音声優先フォーマット選択。`bestaudio` が無いサイトでは `best`
/// へ縮退する (音声トラック単体が取れるなら DL/抽出が軽くなり、無ければ
/// 従来どおり最良ストリームを取得する)。
const AUDIO_FORMAT_SELECTOR: &str = "bestaudio/best";

/// 断片並列ダウンロードの並列数 (`--concurrent-fragments`)。HLS/DASH の
/// fragment を並列取得して DL スループットを上げる。4 は単発リンク向けに
/// 過剰でなく、断片化されたストリームでは体感が大きく改善する妥当値。
const CONCURRENT_FRAGMENTS: u8 = 4;

/// 単一 TCP read のタイムアウト秒 (`--socket-timeout`)。サーバ無応答 / ストール時に
/// yt-dlp が 1 ソケット読みで無限ブロックするのを断つ (A2: resolving が遅い対策)。
const YTDLP_SOCKET_TIMEOUT_SECS: u32 = 20;

/// HTTP リクエスト全体のリトライ回数 (`--retries`)。一過性のネットワーク失敗で
/// 即座に諦めず数回リトライして「解決中で固まる」体感を減らす (A2)。
const YTDLP_RETRIES: u32 = 3;

/// fragment (HLS/DASH セグメント) 単位のリトライ回数 (`--fragment-retries`)。
/// 断片取得の一過性失敗を吸収する (A2)。
const YTDLP_FRAGMENT_RETRIES: u32 = 3;

/// yt-dlp の音声抽出 (`ExtractAudio`) 後処理が内部で起動する ffmpeg へ渡す追加
/// 引数 (`--postprocessor-args`)。
///
/// ## なぜこれが必要か (Slice D: 二重エンコード解消)
/// yt-dlp の `--extract-audio --audio-format mp3` は **1 回だけ** ffmpeg を起動し
/// MP3 へエンコードする。しかしその出力はソース由来の sample_rate / channels の
/// ままで、44100Hz / 2ch とは限らず、ソースのメタデータも残り得る。
///
/// 従来はこの後段で core 側が `convert_media_blocking`
/// (`libmp3lame -ar 44100 -ac 2 -map_metadata -1`) を再度かけて正規化・メタ除去
/// していたが、これは **2 回目の MP3 エンコード** であり音質劣化・時間増を招いた。
///
/// 本スライスでは、その正規化を yt-dlp の **単一の** ExtractAudio エンコード
/// パスに畳み込む。`ExtractAudio:` でスコープを音声抽出後処理に限定し、その
/// ffmpeg 呼び出しに `-ar 44100 -ac 2 -map_metadata -1` を渡すことで、
/// core 側の再エンコード無しに「44100Hz / 2ch 正規化済み・メタ除去済み MP3」を
/// **エンコード 1 回だけ** で得る。これにより分割時は再エンコードせず
/// `-c copy` セグメント (lib.rs `segment_mp3_copy`) で済む。
///
/// `core` の正規化ポリシー (`base_audio_args`: `-ar 44100 -ac 2 -map_metadata -1`)
/// と引数の意味付けを一致させてある。
/// optimize 時は既存 MP3 の stream copy を避けるため `best` を取得し、後段で
/// 64kbps / 24kHz / mono MP3 へ一度だけ変換する。この通常経路の説明は維持する。
const EXTRACT_AUDIO_NORMALIZE_PPARGS: &str = "ExtractAudio:-ar 44100 -ac 2 -map_metadata -1";
const OPTIMIZED_MP3_BITRATE: &str = "64k";
const OPTIMIZED_MP3_SAMPLE_RATE: &str = "24000";

/// yt-dlp プロセス全体の内部デッドライン (応答性のための無限ブロック解消)。
///
/// `child.wait()` を素で待つと、yt-dlp が抽出器の待機・ネットワークハングで
/// 永久にブロックし、CLI / MCP / host 呼び出し側まで応答不能になる。これを
/// 防ぐため core 内部で deadline を持ち、超過時はプロセスを kill してエラー化
/// する。
///
/// ## 値の根拠
/// 30 分。講座系の長尺動画 (1〜2 時間級) でも、`bestaudio` 音声トラックのみを
/// 取得し並列断片 DL する実運用では十分間に合う一方、抽出器ハングや無応答
/// ネットワークを 30 分以上放置しないための保守的な上限。長尺の正常 DL を
/// 誤って中断しない十分長い値であり、かつ無限ブロックは確実に断つ。
///
/// 将来 MCP / CLI 引数から上書きできるよう、待機処理は
/// [`run_ytdlp_with_deadline`] に分離してある (本スライスでは内部デフォルト
/// のみ。引数契約は変更しない)。
const YTDLP_PROCESS_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// deadline 超過後、kill したプロセスのゾンビ回収 (`wait`) を諦めるまでの猶予。
/// kill 後はほぼ即座に終了するため短くてよい。回収に失敗しても DL 自体は
/// 失敗エラーで返すため致命的ではない。
const YTDLP_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// deadline 監視のためにプロセス終了をポーリングする間隔。
const YTDLP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A1: yt-dlp が無出力のまま経過したとき、UI が「固まった」ように見えないよう
/// heartbeat 進捗を送る間隔。これ未満の無音は通知しない (過剰通知防止)。
const YTDLP_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);

/// yt-dlp 経路の失敗種別。
///
/// 後続 Slice 5 (認証 auth helper) が **認証起因の失敗を機械的に検知** できるよう、
/// エラーを種別化する。`Display` は日本語のユーザ向け文言を返すが、種別判定は
/// `kind()` / `is_auth_error()` を使う (文言の i18n と分離するため)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YtDlpErrorKind {
    /// yt-dlp バイナリが解決できない (未同梱 + PATH/Homebrew にも無い)。
    BinaryNotFound,
    /// 認証が必要 / 失敗 (login required / HTTP 401 / 403 / "Sign in" 等)。
    /// Slice 5 の認証 auth helper はこの種別を見て再認証フローへ分岐する。
    AuthRequired,
    /// 抽出器が対応していない / 本体 URL を解決できない。
    Unsupported,
    /// 公開動画だが配信側 (YouTube 等) に取得をブロックされた / 抽出が失敗した。
    ///
    /// 例: YouTube の「Sign in to confirm you're not a bot」(=ボット確認。個別
    /// ログインは不要)、`No video formats found` などの抽出失敗。これらは個別
    /// アカウントの認証情報では解決できないため、[`AuthRequired`] と区別して
    /// **login window / auth helper を起動させない** (Slice H)。
    ///
    /// [`AuthRequired`]: YtDlpErrorKind::AuthRequired
    ExtractionBlocked,
    /// 上記以外の実行時失敗 (ネットワーク・I/O・yt-dlp 内部エラー等)。
    Other,
}

/// yt-dlp 経路のエラー。種別 + ユーザ向け文言を持つ。
#[derive(Debug, Clone)]
pub struct YtDlpError {
    kind: YtDlpErrorKind,
    message: String,
}

impl YtDlpError {
    /// 種別 + ユーザ向け文言からエラーを作る。
    ///
    /// core 内部の分類で使うほか、呼び出し側 (MCP の auth helper 連携など) が yt-dlp
    /// 後段の失敗 (分割・metadata 取得) を `Other` 種別へ包み直す際にも使う。
    /// 文言には cookie / token などの秘密を含めないこと (呼び出し側責務)。
    pub fn new(kind: YtDlpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// 失敗種別。
    pub fn kind(&self) -> &YtDlpErrorKind {
        &self.kind
    }

    /// 認証起因の失敗かどうか (Slice 5 が利用する判定)。
    pub fn is_auth_error(&self) -> bool {
        self.kind == YtDlpErrorKind::AuthRequired
    }
}

impl std::fmt::Display for YtDlpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for YtDlpError {}

/// 進捗送信失敗 (`ProgressFn` が `Err` を返した) を `Other` 種別へ包む。
impl From<YtDlpError> for String {
    fn from(error: YtDlpError) -> String {
        error.message
    }
}

/// yt-dlp 呼び出しオプション。
///
/// `Default` は `optimize_for_size = false` (高音質 `--audio-quality 2`)、
/// `user_agent = None` ([`DEFAULT_USER_AGENT`] を使用)。
#[derive(Debug, Clone, Default)]
pub struct YtDlpOptions {
    /// `true` なら文字起こし向けの 64kbps / 24kHz / mono MP3 を一度だけ生成する。
    /// `false` なら既定の高音質 (`--audio-quality 2`)。
    pub optimize_for_size: bool,
    /// 上書きする User-Agent。`None` なら [`DEFAULT_USER_AGENT`]。
    pub user_agent: Option<String>,
}

/// `Cookie:` ヘッダ文字列 (`name=value; name2=value2; ...`) を yt-dlp が要求する
/// Netscape cookie file 形式の文字列へ変換する。
///
/// ## 制約 (重要)
/// ヘッダ文字列からは cookie の `domain` / `path` / `expires` / `secure` /
/// `HttpOnly` 属性を **完全には復元できない**。ここでは対象 URL から妥当な
/// 値を補完する:
/// - `domain`: 対象 URL のホスト。先頭ドット付き (`.example.com`) にして
///   サブドメインにもマッチさせる (実績の Loom / Utage 系で安全側)。
/// - `include_subdomains` フラグ: 先頭ドット付きなので `TRUE`。
/// - `path`: `/` 固定 (ヘッダからは取得不能なため最も広い安全値)。
/// - `secure`: 対象 URL が https なら `TRUE`、それ以外 `FALSE`。
/// - `expires`: `0` (セッション cookie 相当)。yt-dlp はこれを受理する。
///
/// この補完は「取得できない属性は yt-dlp が受理する安全なデフォルトに倒す」
/// 方針であり、cookie のスコープを意図せず狭めない (むしろ広めに倒す) ことを
/// 優先している。`domain` 補完を `.<host>` (include_subdomains=TRUE) に倒すのも
/// 同じ理由で、ここを過剰に絞らない (狭めると対象 cookie が yt-dlp に渡らず
/// 認証に失敗する) のが Slice 3 レビューで確定した意図である。
///
/// ## 属性混在の防御 (F4)
/// 純粋な `Cookie:` リクエストヘッダには cookie 属性 (`Path` / `Domain` /
/// `Expires` / `Max-Age` / `SameSite` / `Secure` / `HttpOnly`) は来ない。ただし
/// 呼び出し側が誤って `Set-Cookie:` 由来の文字列を渡した場合に備え、これらの
/// 属性キー (大文字小文字無視) は cookie 名として出力しないよう防御的に弾く。
pub fn cookie_header_to_netscape(cookie_header: &str, target: &Url) -> String {
    let host = target.host_str().unwrap_or("");
    // 先頭ドット付きドメイン。host が空 (file URL 等) の場合は空のまま。
    let domain = if host.is_empty() {
        String::new()
    } else if host.starts_with('.') {
        host.to_string()
    } else {
        format!(".{host}")
    };
    let include_subdomains = if domain.is_empty() { "FALSE" } else { "TRUE" };
    let secure = if target.scheme() == "https" {
        "TRUE"
    } else {
        "FALSE"
    };

    let mut out = String::new();
    out.push_str("# Netscape HTTP Cookie File\n");
    // yt-dlp / curl はこのマーカー行で書式を識別する。
    out.push_str("# This file is generated by revmagic-core. Do not edit.\n");

    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name.trim(), value.trim()),
            // `=` を含まない属性的トークン (まれ) はスキップ。
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        // F4: cookie 属性キー (Set-Cookie 由来の混入) を cookie 名として出さない。
        if is_cookie_attribute(name) {
            continue;
        }
        // A10: Netscape 形式は TAB 区切り・改行終端なので、name/value に TAB / CR / LF が
        // 含まれると 7 フィールド構造を壊し、行注入 (cookie file injection) を許す。
        // 制御文字を含む pair は安全側に skip する (yt-dlp へ渡る cookie が 1 つ欠けても、
        // 不正な jar を作るより安全)。
        if has_control_char(name) || has_control_char(value) {
            continue;
        }
        // Netscape 形式は TAB 区切り 7 フィールド:
        // domain  include_subdomains  path  secure  expires  name  value
        out.push_str(&format!(
            "{domain}\t{include_subdomains}\t/\t{secure}\t0\t{name}\t{value}\n"
        ));
    }

    out
}

/// `name` が cookie 属性キー (`Set-Cookie` 由来の混入) かどうか。大文字小文字無視。
///
/// 純粋な `Cookie:` ヘッダには来ないが、防御的に弾く (F4)。
fn is_cookie_attribute(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "path" | "domain" | "expires" | "max-age" | "samesite" | "secure" | "httponly"
    )
}

/// A10: 文字列に制御文字 (TAB / CR / LF を含む ASCII 制御 / Unicode 制御) が
/// 含まれるか。Netscape cookie file の TAB 区切り・改行終端を壊して行注入を許す
/// pair を弾くために使う。
fn has_control_char(value: &str) -> bool {
    value.chars().any(|c| c.is_control())
}

/// 0600 の一時 cookie file を作成し、Netscape 形式で `contents` を書き込む。
///
/// 返すのは [`TempCookieFile`] で、`Drop` 時にファイルを削除する (パスや中身は
/// ログに出さない)。作成に失敗した場合は `Other` 種別のエラーを返す。
struct TempCookieFile {
    path: PathBuf,
}

impl TempCookieFile {
    fn create(contents: &str) -> Result<Self, YtDlpError> {
        let base = std::env::temp_dir().join("revmagic-cookies");
        fs::create_dir_all(&base).map_err(|error| {
            YtDlpError::new(
                YtDlpErrorKind::Other,
                format!("cookie 一時フォルダを作成できませんでした: {error}"),
            )
        })?;
        // A11/F3: cookie 一時ディレクトリは 0700 (所有者のみ)。file は 0600。
        // auth helper.json 同等の fail-closed 検証を行う: ディレクトリを 0700 へ締めたうえで、
        // lstat で (1) symlink でない実ディレクトリ、(2) 所有者が現在の euid、
        // (3) mode が 0700 (group/other ビット無し) を検証し、満たさなければエラーで
        // 中断する。共有 /tmp 上で他ユーザが先回りして作ったディレクトリ / symlink を
        // 経由して cookie を書く事故を防ぐ。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // best-effort で 0700 に締める (既存ディレクトリの緩いパーミッションを是正)。
            let _ = fs::set_permissions(&base, fs::Permissions::from_mode(0o700));
            harden_cookie_dir(&base)?;
        }

        // A11: ファイル名を予測困難化する。timestamp だけだと並列 / 既知粒度で予測され
        // 得るため、PID・nanos・プロセス内単調カウンタを連結する (O_EXCL 失敗時は再試行)。
        // 推測可能名 + best-effort perms を、ランダム名 + O_EXCL create_new + 0600 へ強化。
        let file;
        let path;
        let mut attempt = 0u32;
        loop {
            let candidate = base.join(format!("cookies-{}.txt", random_cookie_token()));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&candidate) {
                Ok(opened) => {
                    file = opened;
                    path = candidate;
                    break;
                }
                // 既存名衝突 (極めて稀) は別名で再試行する。それ以外は即エラー。
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 8 => {
                    attempt += 1;
                    continue;
                }
                Err(error) => {
                    return Err(YtDlpError::new(
                        YtDlpErrorKind::Other,
                        format!("cookie 一時ファイルを作成できませんでした: {error}"),
                    ));
                }
            }
        }
        // F3: ファイル open 直後に Drop guard を構築する。以降の write/flush が
        // 失敗しても guard の Drop が確実にファイルを削除する (失敗経路で残骸を
        // 残さない)。
        let guard = Self { path };
        let mut file = file;
        file.write_all(contents.as_bytes()).map_err(|error| {
            YtDlpError::new(
                YtDlpErrorKind::Other,
                format!("cookie 一時ファイルへの書き込みに失敗しました: {error}"),
            )
        })?;
        file.flush().map_err(|error| {
            YtDlpError::new(
                YtDlpErrorKind::Other,
                format!("cookie 一時ファイルの反映に失敗しました: {error}"),
            )
        })?;
        Ok(guard)
    }
}

impl Drop for TempCookieFile {
    fn drop(&mut self) {
        // best-effort 削除。失敗してもログには出さない (パス露出回避)。
        let _ = fs::remove_file(&self.path);
    }
}

/// A11: cookie 一時ディレクトリを fail-closed 検証する (Unix)。
///
/// `lstat` で symlink を辿らずに `base` を検査し、以下を要求する:
/// - 通常ディレクトリである (symlink / 他種別ではない)。
/// - 所有者 (uid) が現在の実効 uid に一致する (他ユーザ所有の dir を経由しない)。
/// - mode の group/other ビットが立っていない (= 0700 相当、所有者のみ)。
///
/// 1 つでも満たさなければエラーを返し、cookie をそこへ書かない (秘密漏洩防止)。
/// パスや mode の生値はエラー文言へ出さない (F1: パス露出回避)。
#[cfg(unix)]
fn harden_cookie_dir(base: &Path) -> Result<(), YtDlpError> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::symlink_metadata(base).map_err(|error| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            format!("cookie 一時フォルダを検証できませんでした: {error}"),
        )
    })?;

    // symlink / 非ディレクトリは拒否 (symlink 経由の書き換え攻撃を断つ)。
    if !meta.is_dir() {
        return Err(YtDlpError::new(
            YtDlpErrorKind::Other,
            "cookie 一時フォルダが不正です (ディレクトリではありません)。",
        ));
    }
    // 所有者が現在の実効 uid であること。
    // SAFETY: geteuid は引数なし副作用なしの純粋なゲッタ。
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(YtDlpError::new(
            YtDlpErrorKind::Other,
            "cookie 一時フォルダの所有者が不正です。",
        ));
    }
    // group/other に一切ビットが立っていないこと (0700 相当)。
    if meta.mode() & 0o077 != 0 {
        return Err(YtDlpError::new(
            YtDlpErrorKind::Other,
            "cookie 一時フォルダのパーミッションが緩すぎます。",
        ));
    }
    Ok(())
}

/// A11: 予測困難な cookie ファイル名トークンを生成する (`rand` 依存なし)。
///
/// PID・高分解能タイムスタンプ (nanos)・プロセス内単調カウンタを連結する。
/// O_EXCL (`create_new`) と併用するため衝突しても作成は失敗 → 別トークンで再試行
/// するので、ここでは「実質的に当てにくい」程度で十分 (暗号学的乱数は不要)。
fn random_cookie_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("{pid:x}-{nanos:x}-{seq:x}")
}

/// yt-dlp で `url` の音声を抽出し MP3 として `output_dir` に保存する (blocking)。
///
/// 呼び出し側は `spawn_blocking` 等でブロッキング実行することを想定する。
/// 戻り値は生成された MP3 の絶対パス。
///
/// - `cookie`: `Cookie:` ヘッダ文字列。`Some` なら Netscape cookie file に変換し
///   `--cookies` で渡す (argv に値は載らない)。空 / 空白なら無視。
/// - `referer`: `--referer` に渡す。`None` なら対象 URL のオリジンを補完。
/// - `options`: 音質 / UA。
pub fn extract_audio_via_ytdlp<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: &str,
    output_dir: &Path,
    cookie: Option<&str>,
    referer: Option<&str>,
    options: &YtDlpOptions,
    progress: ProgressFn<'_>,
) -> Result<String, YtDlpError> {
    extract_audio_via_ytdlp_with_deadline(
        resolver,
        url,
        output_dir,
        cookie,
        referer,
        options,
        progress,
        YTDLP_PROCESS_DEADLINE,
    )
}

/// [`extract_audio_via_ytdlp`] の deadline 明示版。
///
/// yt-dlp プロセスが `deadline` を超えても完了しない場合は kill + ゾンビ回収し、
/// secret を含まない固定文言の `Other` エラーを返す (無限ブロック解消)。
///
/// 既定の公開 API ([`extract_audio_via_ytdlp`]) は内部デフォルト
/// [`YTDLP_PROCESS_DEADLINE`] でこれを呼ぶ。将来 MCP / CLI から deadline を
/// 上書きしたい場合の差し込み点として公開している (本スライスでは引数契約は
/// 変更せず、内部デフォルトのみ運用)。
#[allow(clippy::too_many_arguments)]
pub fn extract_audio_via_ytdlp_with_deadline<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: &str,
    output_dir: &Path,
    cookie: Option<&str>,
    referer: Option<&str>,
    options: &YtDlpOptions,
    progress: ProgressFn<'_>,
    deadline: Duration,
) -> Result<String, YtDlpError> {
    // 後方互換: cancel トークン無し (None) で cancel 対応版へ委譲する。
    extract_audio_via_ytdlp_with_deadline_cancel(
        resolver, url, output_dir, cookie, referer, options, progress, deadline, None,
    )
}

/// [`extract_audio_via_ytdlp_with_deadline`] の協調的キャンセル対応版
/// (Slice I-1 reviewer fix / 指摘2)。
///
/// `cancel` に `Some(flag)` を渡すと、yt-dlp 進捗ポーリングループが
/// 各 `recv_timeout` 周回 (100ms 粒度) でフラグを確認し、立っていれば
/// プロセスグループごと kill + ゾンビ回収して即座に中断エラーを返す
/// (無音ハングでも ~100ms で停止)。`None` なら従来挙動と同一。
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_audio_via_ytdlp_with_deadline_cancel<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: &str,
    output_dir: &Path,
    cookie: Option<&str>,
    referer: Option<&str>,
    options: &YtDlpOptions,
    progress: ProgressFn<'_>,
    deadline: Duration,
    cancel: Cancel<'_>,
) -> Result<String, YtDlpError> {
    let parsed = Url::parse(url).map_err(|error| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            format!("URL を解析できませんでした: {error}"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(YtDlpError::new(
            YtDlpErrorKind::Other,
            "http または https の URL を指定してください。",
        ));
    }

    if !output_dir.exists() {
        return Err(YtDlpError::new(
            YtDlpErrorKind::Other,
            "保存先フォルダが見つかりません。",
        ));
    }

    let ytdlp = resolver
        .resolve("yt-dlp")
        .map_err(|message| YtDlpError::new(YtDlpErrorKind::BinaryNotFound, message))?;

    progress_or(
        progress,
        &Progress {
            percent: 0,
            step: "解決中".into(),
            detail: "埋め込み動画の本体を解決しています。".into(),
            current_seconds: 0.0,
            total_seconds: 0.0,
        },
    )?;

    // cookie file は cookie がある時だけ作る。Drop で必ず削除される。
    let cookie_file = match cookie.map(str::trim).filter(|value| !value.is_empty()) {
        Some(header) => {
            let netscape = cookie_header_to_netscape(header, &parsed);
            Some(TempCookieFile::create(&netscape)?)
        }
        None => None,
    };

    let user_agent = options
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_USER_AGENT);

    let referer_value = referer
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| match parsed.host_str() {
            Some(host) => format!("{}://{}/", parsed.scheme(), host),
            None => format!("{}://", parsed.scheme()),
        });

    let audio_quality = if options.optimize_for_size { "3" } else { "2" };

    // A4/A5/A7: yt-dlp の生成物 (生 MP3) を出力 dir 直下に散らかさず、専用の job
    // folder `<output_dir>/<stem>-mp3-<ts>/` に隔離する。これにより
    // (1) 出力先にフォルダと生 MP3 が両方生まれる問題が消え (A4)、
    // (2) `resolve_output_mp3` が job folder 内だけを走査するので「最新 mp3」誤検出 /
    //     並列実行の取り違えが解消する (A7)。
    // job folder 名は他経路 (ローカル / 直リンク変換) と同じ命名規則
    // (`create_ytdlp_job_folder`) に揃える。
    let job_folder = create_ytdlp_job_folder(output_dir)
        .map_err(|message| YtDlpError::new(YtDlpErrorKind::Other, message))?;

    // A13: 出力テンプレートを `%(title,id).200B.%(ext)s` にする。
    // - `title,id`: title が空のとき id へフォールバックし、空タイトルでも `.mp3`
    //   隠しファイル化 (検出不能) を防ぐ。
    // - `.200B`: title 部を 200 byte に制限し、超長名による失敗を防ぐ。
    // `--restrict-filenames` (build_ytdlp_args) と併せて OS 予約 / 制御文字も除去する。
    let output_template = job_folder.join("%(title,id).200B.%(ext)s");

    // 修正2 (Slice H): 同梱 ffmpeg のディレクトリを解決し `--ffmpeg-location` で
    // yt-dlp へ明示する。GUI (Finder/Dock 起動) は最小 PATH で動くため、PATH 上の
    // ffmpeg に依存すると音声抽出 (ExtractAudio) が失敗し得る。同梱 ffmpeg の絶対
    // パスを解決し、その**親ディレクトリ**を渡す。解決失敗時は付与せず従来どおり
    // yt-dlp の PATH 探索へ fallback する (補完 PATH に ffmpeg があれば拾える)。
    let ffmpeg_dir = resolver
        .resolve("ffmpeg")
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));

    let cookie_path = cookie_file.as_ref().map(|f| f.path.as_path());
    let args = build_ytdlp_args_with_audio_format(
        user_agent,
        &referer_value,
        audio_quality,
        if options.optimize_for_size {
            "best"
        } else {
            "mp3"
        },
        cookie_path,
        ffmpeg_dir.as_deref(),
        &output_template,
        parsed.as_str(),
    );

    // A7: 出力 MP3 の解決は **job folder 内** だけを走査する (出力 dir 全体ではない)。
    let resolved_path = match run_ytdlp_with_deadline(
        &ytdlp,
        args,
        &job_folder,
        cookie_path,
        progress,
        deadline,
        cancel,
    ) {
        Ok(path) => path,
        Err(error) => {
            // 失敗時は空 (または不完全な) job folder が残らないよう掃除する (best-effort)。
            let _ = fs::remove_dir_all(&job_folder);
            return Err(error);
        }
    };

    progress_or(
        progress,
        &Progress {
            percent: 100,
            step: "抽出完了".into(),
            detail: "MP3 を生成しました。".into(),
            current_seconds: 0.0,
            total_seconds: 0.0,
        },
    )?;

    // cookie_file はここで Drop され削除される (明示 drop で順序を固定)。
    drop(cookie_file);

    Ok(resolved_path)
}

/// yt-dlp の argv を組み立てる。cookie 値そのものは載らない (`--cookies` は
/// ファイルパスのみ)。テスト可能なように分離している。
///
/// `pub` で公開しているのは、二重エンコード解消 (Slice D) の引数レベル検証
/// (`tests/split_no_reencode.rs`) が「yt-dlp 単一パスで正規化
/// (`--postprocessor-args ExtractAudio:-ar 44100 -ac 2 -map_metadata -1`) が
/// 担保される argv になっている」ことを deterministic に確認するため。cookie
/// 値は引数に含まれない (パスのみ) ので公開しても秘密は露出しない。
///
/// `ffmpeg_dir` (修正2 / Slice H): `Some` なら `--ffmpeg-location <dir>` を挿入し、
/// yt-dlp の ExtractAudio 後処理が PATH 探索ではなく同梱 ffmpeg を使うようにする。
/// `None` なら付与せず従来どおり PATH 探索へ fallback する。
pub fn build_ytdlp_args(
    user_agent: &str,
    referer: &str,
    audio_quality: &str,
    cookie_file: Option<&Path>,
    ffmpeg_dir: Option<&Path>,
    output_template: &Path,
    url: &str,
) -> Vec<OsString> {
    build_ytdlp_args_with_audio_format(
        user_agent,
        referer,
        audio_quality,
        "mp3",
        cookie_file,
        ffmpeg_dir,
        output_template,
        url,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_ytdlp_args_with_audio_format(
    user_agent: &str,
    referer: &str,
    audio_quality: &str,
    audio_format: &str,
    cookie_file: Option<&Path>,
    ffmpeg_dir: Option<&Path>,
    output_template: &Path,
    url: &str,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("--user-agent"),
        OsString::from(user_agent),
        OsString::from("--referer"),
        OsString::from(referer),
        OsString::from("--no-warnings"),
        OsString::from("--no-playlist"),
        // A2: ソケット無応答 / 一過性ネットワーク失敗で「解決中」のまま固まらないよう、
        // 単一ソケット read のタイムアウトと HTTP/fragment リトライ回数を明示する。
        OsString::from("--socket-timeout"),
        OsString::from(YTDLP_SOCKET_TIMEOUT_SECS.to_string()),
        OsString::from("--retries"),
        OsString::from(YTDLP_RETRIES.to_string()),
        OsString::from("--fragment-retries"),
        OsString::from(YTDLP_FRAGMENT_RETRIES.to_string()),
        // 音声優先フォーマット選択。bestaudio が無いサイトは best へ縮退する。
        OsString::from("-f"),
        OsString::from(AUDIO_FORMAT_SELECTOR),
        // 断片並列 DL でスループットを上げる (HLS/DASH の fragment 取得を並列化)。
        OsString::from("--concurrent-fragments"),
        OsString::from(CONCURRENT_FRAGMENTS.to_string()),
        // A13: `%(title)s` に OS 予約文字 / 制御文字 / 過剰長が混入すると core の
        // sanitize_name をバイパスして不正ファイル名になり得る。`--restrict-filenames`
        // で yt-dlp 側が ASCII 安全名へ正規化し、出力テンプレートの title 長も
        // `%(title).200B` (下記) で 200 byte に制限する。
        OsString::from("--restrict-filenames"),
        OsString::from("--extract-audio"),
        OsString::from("--audio-format"),
        OsString::from(audio_format),
        OsString::from("--audio-quality"),
        OsString::from(audio_quality),
        // Slice D: 単一の ExtractAudio エンコードで 44100Hz/2ch 正規化・メタ除去まで
        // 行う。これにより後段の二重エンコードが不要になり、分割は -c copy で済む。
        OsString::from("--postprocessor-args"),
        OsString::from(EXTRACT_AUDIO_NORMALIZE_PPARGS),
        OsString::from("--newline"),
        OsString::from("--output"),
        output_template.as_os_str().into(),
    ];

    if let Some(path) = cookie_file {
        args.push(OsString::from("--cookies"));
        args.push(path.as_os_str().into());
    }

    // 修正2 (Slice H): 同梱 ffmpeg の場所を明示し PATH 依存を断つ。
    if let Some(dir) = ffmpeg_dir {
        args.push(OsString::from("--ffmpeg-location"));
        args.push(dir.as_os_str().into());
    }

    args.push(OsString::from(url));
    args
}

/// yt-dlp を起動し、stdout の `--newline` 進捗をパースして `progress` に通知し、
/// 終了後に出力 MP3 のパスを解決する。`deadline` を超えてもプロセスが終了しない
/// 場合は kill + ゾンビ回収し、redact 済み (固定文言) の `Other` エラーを返す
/// (無限ブロック解消)。stdout 進捗ループと終了待機の双方で deadline を監視する。
#[allow(clippy::too_many_arguments)]
fn run_ytdlp_with_deadline(
    ytdlp: &Path,
    args: Vec<OsString>,
    output_dir: &Path,
    cookie_file: Option<&Path>,
    progress: ProgressFn<'_>,
    deadline: Duration,
    cancel: Cancel<'_>,
) -> Result<String, YtDlpError> {
    let started = Instant::now();
    let mut command = Command::new(ytdlp);
    command
        .args(args)
        // SIGTTIN 根絶: process_group(0) で独立グループ化した child が制御端末(tty)を
        // stdin から読もうとすると SIGTTIN で停止しハングする。stdin を /dev/null に
        // 差し替えて端末継承を断つ。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 修正1 (Slice H): yt-dlp child の PATH を補完する。GUI (Finder/Dock 起動) は
    // 最小 PATH (`/usr/bin:/bin:/usr/sbin:/sbin`) で動くため、yt-dlp が YouTube の
    // JS チャレンジ解決に使う deno/node や音声抽出に使う ffmpeg を PATH 上に見つけ
    // られず、公開動画でも失敗し得る。継承 PATH に実在する共通ツールディレクトリを
    // 重複なく連結して child へ渡す (信頼できる絶対ディレクトリのみ)。
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let base = std::env::var_os("PATH");
        command.env("PATH", augmented_path(base, &home));
    }
    // 指摘1(a): yt-dlp を **新しいプロセスグループ** のリーダーとして起動する
    // (Unix)。yt-dlp は抽出/変換で ffmpeg 等の子孫プロセスを spawn し、その子孫が
    // stdout/stderr の FD を握ったまま残ると、直下プロセスだけを kill しても
    // pipe reader が EOF を受け取れず join() で再ハングする。プロセスグループを
    // 分離しておけば、deadline 超過時に `killpg` でグループごと (= 子孫ごと) 殺せ、
    // FD が解放されて reader が EOF に到達する。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 自分を pgid とする新グループ (setpgid(0, 0) 相当)。
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            format!("yt-dlp の実行に失敗しました: {error}"),
        )
    })?;

    let stdout = child.stdout.take().ok_or_else(|| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            "yt-dlp の進捗出力を取得できませんでした。",
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            "yt-dlp のエラー出力を取得できませんでした。",
        )
    })?;

    // stderr は別スレッドで吸い出してエラー分類に使う (秘密は載らない想定だが、
    // 呼び出し側でログ化する際は redact 前提)。
    let stderr_handle = std::thread::spawn(move || -> String {
        let mut buffer = String::new();
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    buffer.push_str(&line);
                    buffer.push('\n');
                }
                Err(_) => break,
            }
        }
        buffer
    });

    // stdout も別スレッドで読み、行をチャネルへ送る。こうすることで「yt-dlp が
    // 無出力のままハングする」ケースでも main スレッドが read で無限ブロックせず、
    // deadline をポーリングできる。progress コールバックは Send でない可能性が
    // あるため main スレッド側で発火させる (ここでは行を受け取るだけ)。
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel::<String>();
    let stdout_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    // 受信側が drop 済み (deadline で先に抜けた等) なら送信失敗で終了。
                    if stdout_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut last_percent: u8 = 0;
    // A1: 無出力 heartbeat 用に「最後に何か通知 / 受信した時刻」を持つ。出力が
    // 一定時間 (YTDLP_HEARTBEAT_INTERVAL) 途切れたら、経過秒を detail に載せた
    // heartbeat を送り、UI が固まって見えないようにする。
    let mut last_output_at = Instant::now();
    // ダウンロード fragment と後処理 (ffmpeg 抽出) の二段。DL を 0-90%、
    // 後処理を 90-99% に大まかにマップする。
    // deadline 監視: 行受信を POLL_INTERVAL でタイムアウトさせ、毎周回で
    // 経過時間とプロセス終了を確認する。これにより stdout が無音でも deadline
    // 超過を検知できる。
    let process_result = loop {
        if started.elapsed() >= deadline {
            // deadline 超過 → プロセスグループごと kill してゾンビ回収し、明示エラーへ。
            kill_and_reap(&mut child);
            // 指摘1(b): reader スレッドの join は **bounded** にする。group kill で
            // 子孫まで殺し FD を解放するため通常はすぐ EOF で終了するが、万一 FD が
            // 残っても join() で再ハングしないよう、短い猶予後に諦めて detach する。
            bounded_join(stdout_handle);
            let _ = bounded_join_string(stderr_handle);
            return Err(deadline_error());
        }

        // 指摘2: 各ポーリング周回 (recv_timeout 100ms 粒度) でキャンセルトークンを
        // 確認する。立っていれば **進捗通知を待たずに** プロセスグループごと kill +
        // ゾンビ回収して即座に中断する。これにより yt-dlp が無音 (出力ゼロ) のまま
        // ハングするケースでも deadline (最大 30 分) を待たず ~100ms 以内で停止する。
        if is_cancelled(cancel) {
            kill_and_reap(&mut child);
            bounded_join(stdout_handle);
            let _ = bounded_join_string(stderr_handle);
            return Err(cancel_error());
        }

        // 残り時間と POLL_INTERVAL の小さい方で受信待ち。
        let remaining = deadline.saturating_sub(started.elapsed());
        let wait = remaining.min(YTDLP_POLL_INTERVAL);
        match stdout_rx.recv_timeout(wait) {
            Ok(line) => {
                // 進捗コールバックが `Err` を返す = 呼び出し側のキャンセル / channel 切断。
                // この場合は単に return せず、deadline 超過時と同じく **プロセスグループ
                // ごと kill + ゾンビ回収** してから中断エラーを返す。素の `?` で抜けて
                // `Child` を drop するだけでは Unix では child が kill されず (新プロセス
                // グループのリーダーなので) 子孫ごと走り続けるため、キャンセルが効かない。
                // A1: 何らかの行を受信した = yt-dlp が生きて進んでいる。heartbeat タイマを
                // リセットする (無音判定の起点を更新)。
                last_output_at = Instant::now();
                let notified = if let Some(percent) = parse_ytdlp_download_percent(&line) {
                    // DL 部分は 0-90% にスケール。
                    let scaled = ((percent * 0.9).round() as u8).clamp(1, 90);
                    if scaled > last_percent {
                        last_percent = scaled;
                        Some(progress_or(
                            progress,
                            &Progress {
                                percent: scaled,
                                step: "ダウンロード中".into(),
                                detail: "埋め込み動画を取得しています。".into(),
                                current_seconds: 0.0,
                                total_seconds: 0.0,
                            },
                        ))
                    } else {
                        None
                    }
                } else if let Some(resolve_pct) = parse_ytdlp_resolve_percent(&line) {
                    // A1: DL 前の解決 / 抽出フェーズを 1-10% にマップし、「解決中」で 0% の
                    // まま固まって見えるのを防ぐ。DL (0-90%) が始まったら last_percent が
                    // 追い越すのでここは発火しなくなる (単調増加を保つ)。
                    if resolve_pct > last_percent && last_percent < 10 {
                        last_percent = resolve_pct;
                        Some(progress_or(
                            progress,
                            &Progress {
                                percent: resolve_pct,
                                step: "解決中".into(),
                                detail: "埋め込み動画の本体を解決しています。".into(),
                                current_seconds: 0.0,
                                total_seconds: 0.0,
                            },
                        ))
                    } else {
                        None
                    }
                } else if (line.contains("[ExtractAudio]") || line.contains("[ffmpeg]"))
                    && last_percent < 95
                {
                    last_percent = 95;
                    Some(progress_or(
                        progress,
                        &Progress {
                            percent: 95,
                            step: "音声抽出中".into(),
                            detail: "MP3 へ変換しています。".into(),
                            current_seconds: 0.0,
                            total_seconds: 0.0,
                        },
                    ))
                } else {
                    None
                };
                if let Some(Err(error)) = notified {
                    kill_and_reap(&mut child);
                    bounded_join(stdout_handle);
                    let _ = bounded_join_string(stderr_handle);
                    return Err(error);
                }
            }
            // この POLL 周期では新しい行が無い。プロセスが既に終了していれば抜ける。
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    break Ok(status);
                }
                // A1: 無出力 heartbeat。出力が YTDLP_HEARTBEAT_INTERVAL 以上途切れたら、
                // 経過秒を載せた進捗を送って「固まっていない」ことを示す。percent は
                // 現在値を維持し (単調性を壊さない)、detail に経過秒を入れる。step は
                // フェーズに応じて切り替える: DL がまだ始まっていない (last_percent が
                // 解決フェーズ上限 10% 以下) なら「解決中」、DL 中なら「ダウンロード中」。
                if last_output_at.elapsed() >= YTDLP_HEARTBEAT_INTERVAL {
                    last_output_at = Instant::now();
                    let waited = started.elapsed().as_secs();
                    let step = if last_percent <= 10 {
                        "解決中"
                    } else {
                        "ダウンロード中"
                    };
                    let hb = progress_or(
                        progress,
                        &Progress {
                            percent: last_percent.max(1),
                            step: step.into(),
                            detail: format!("接続中... (経過 {waited} 秒)"),
                            current_seconds: 0.0,
                            total_seconds: 0.0,
                        },
                    );
                    if let Err(error) = hb {
                        kill_and_reap(&mut child);
                        bounded_join(stdout_handle);
                        let _ = bounded_join_string(stderr_handle);
                        return Err(error);
                    }
                }
            }
            // stdout スレッドが pipe 終了で送信を閉じた。プロセス終了を待つ
            // (deadline 付きで)。
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break wait_with_deadline(&mut child, started, deadline);
            }
        }
    };

    // 指摘1(b): ここも bounded join。正常終了でも、yt-dlp が detach した子孫が
    // stdout FD を握ったまま残れば素の join() は無限ブロックし得る。
    bounded_join(stdout_handle);

    let status = match process_result {
        Ok(status) => status,
        // deadline 超過 (wait_with_deadline 経路) → group kill + reap してエラー化。
        Err(WaitOutcome::DeadlineExceeded) => {
            kill_and_reap(&mut child);
            let _ = bounded_join_string(stderr_handle);
            return Err(deadline_error());
        }
        Err(WaitOutcome::WaitFailed(message)) => {
            let _ = bounded_join_string(stderr_handle);
            return Err(YtDlpError::new(YtDlpErrorKind::Other, message));
        }
    };

    let stderr_text = bounded_join_string(stderr_handle);

    if !status.success() {
        return Err(classify_ytdlp_failure(&stderr_text, cookie_file));
    }

    // 出力 MP3 を解決する: `output_dir` 直下で最も新しい .mp3。
    // (テンプレートに %(title)s を使うため事前にファイル名が確定しない)
    resolve_output_audio(output_dir)
}

fn progress_or(progress: ProgressFn<'_>, payload: &Progress) -> Result<(), YtDlpError> {
    progress(payload).map_err(|message| YtDlpError::new(YtDlpErrorKind::Other, message))
}

/// yt-dlp child へ渡す PATH を組み立てる純関数 (修正1 / Slice H)。
///
/// `base` (継承 PATH) に、実在する共通ツールディレクトリ ([`common_tool_dirs`])
/// を **末尾へ重複なく** 連結する。継承 PATH 上のエントリは先頭のまま温存し、
/// 補完分はその後ろに付ける (既存解決を壊さず、見つからない場合のみ補完を効か
/// せる)。
///
/// ## セキュリティ
/// 補完候補は信頼できる絶対ディレクトリのみ。`common_tool_dirs` が実在チェック
/// をしているので存在しない候補は混ざらない。`base` 由来のエントリのうち空文字
/// (POSIX で「カレントディレクトリ」を意味する) と `.` / 相対パスは補完では追加
/// しない。ただし継承 PATH の中身そのものは改変しない (呼び出し側環境の方針を
/// 尊重し、yt-dlp 既存挙動の回帰を避けるため)。
fn augmented_path(base: Option<OsString>, home: &Path) -> OsString {
    // 既存 PATH エントリの集合 (重複排除用)。
    let existing: Vec<PathBuf> = match &base {
        Some(value) => std::env::split_paths(value).collect(),
        None => Vec::new(),
    };

    let mut result = existing.clone();
    for dir in common_tool_dirs(home) {
        if !result.iter().any(|p| p == &dir) {
            result.push(dir);
        }
    }

    // join_paths は空文字を含むと PATH に空エントリ (= カレントディレクトリ) を
    // 残し得るが、補完候補は絶対・実在のみで空は混ざらない。継承分はそのまま温存。
    std::env::join_paths(result).unwrap_or_else(|_| base.unwrap_or_default())
}

/// PATH 補完候補となる共通ツールディレクトリ (実在する絶対パスのみ)。
///
/// deno/node (YouTube の JS チャレンジ解決) や ffmpeg を最小 PATH 環境でも
/// 見つけられるようにするための、信頼できる固定リスト。`.`・相対パス・空文字は
/// 一切含まない。存在しないディレクトリは除外する。
fn common_tool_dirs(home: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".local/share/mise/shims"),
        home.join(".mise/shims"),
        home.join(".asdf/shims"),
    ];
    candidates.retain(|p| p.is_absolute() && p.is_dir());
    candidates
}

/// [`wait_with_deadline`] の失敗種別 (内部用)。
enum WaitOutcome {
    /// deadline を超えてもプロセスが終了しなかった。
    DeadlineExceeded,
    /// `try_wait` 自体が OS エラーで失敗した。
    WaitFailed(String),
}

/// プロセスの終了を deadline までポーリングで待つ。
///
/// - 終了を観測できたら `Ok(status)`。
/// - deadline 超過なら `Err(WaitOutcome::DeadlineExceeded)` (呼び出し側で kill)。
/// - `try_wait` が OS エラーなら `Err(WaitOutcome::WaitFailed)`。
fn wait_with_deadline(
    child: &mut std::process::Child,
    started: Instant,
    deadline: Duration,
) -> Result<std::process::ExitStatus, WaitOutcome> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if started.elapsed() >= deadline {
                    return Err(WaitOutcome::DeadlineExceeded);
                }
                std::thread::sleep(YTDLP_POLL_INTERVAL);
            }
            Err(error) => {
                return Err(WaitOutcome::WaitFailed(format!(
                    "yt-dlp の終了待機に失敗しました: {error}"
                )));
            }
        }
    }
}

/// reader thread の join 上限 (指摘1(b))。group kill 後はほぼ即 EOF で終了する
/// ため短くてよい。猶予を超えても終わらなければ join せず detach する
/// (素の `join()` での再ハングを断つ)。
const YTDLP_READER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// deadline 超過したプロセスを **プロセスグループごと** kill し、ゾンビを回収する
/// (best-effort)。
///
/// 指摘1(a): yt-dlp は ffmpeg 等の子孫を spawn する。直下プロセスだけを kill
/// すると子孫が stdout/stderr FD を握ったまま生き残り、reader thread が EOF に
/// 到達しない。Unix では起動時に新プロセスグループ化してあるので、ここで
/// `killpg(pgid, SIGKILL)` によりグループ (= 子孫を含む) を一括 kill し、FD を
/// 解放させる。pgid は child の pid に等しい (グループリーダー)。
///
/// kill 後は短い猶予 ([`YTDLP_KILL_REAP_TIMEOUT`]) でゾンビ回収を試みる。
/// 回収できなくても DL 自体は失敗エラーで返すため、ここでは結果を無視する。
pub(crate) fn kill_and_reap(child: &mut std::process::Child) {
    // A9 (★PID 再利用安全化): killpg する **前に** 子プロセスが既に終了していないか
    // 確認する。`std::process::Child` は Drop までゾンビを reap しないので、child が
    // 自然終了済み (まだ未 reap = ゾンビ) でも `child.id()` は元の pid を返す。だが、
    // もし呼び出し側が既に `try_wait()` で reap 済みだった場合、その pid (= pgid) は
    // **OS により別プロセスへ再利用され得る**。その状態で `killpg(child.id())` すると
    // 無関係なプロセスグループを巻き込んで SIGKILL してしまう。
    //
    // ここで `try_wait()` し、`Ok(Some(_))` (= 既に終了を観測 = この呼び出しが reap)
    // なら killpg を **行わない**: 子は既に死んでおり、グループへ SIGKILL を送る必要は
    // なく、pgid 再利用の巻き添えリスクだけが残るため。`Ok(None)` (まだ生存) のときだけ
    // killpg + child.kill() を実施する。
    match child.try_wait() {
        // 既に終了を観測 (この try_wait が reap した)。killpg はスキップして安全に戻る。
        Ok(Some(_)) => return,
        // try_wait 自体が失敗。状態不明なので保守的に従来どおり kill を試みる
        // (この経路では reap 済みでないため pid 再利用の可能性は低い)。
        Err(_) => {}
        // まだ生存。下で kill する。
        Ok(None) => {}
    }

    // 指摘1(a): プロセスグループごと SIGKILL (Unix)。グループリーダーの pgid は
    // child の pid。child はまだ未 reap (上の try_wait が Some を返していない) なので、
    // pid はこの child を指し続けており再利用されていない。
    #[cfg(unix)]
    {
        // SAFETY: killpg は副作用のみ。pgid が存在しなければ ESRCH で失敗するが、
        // その場合も以降の child.kill()/try_wait() がフォールバックする。
        let pgid = child.id() as libc::pid_t;
        unsafe {
            let _ = libc::killpg(pgid, libc::SIGKILL);
        }
    }
    // 直下プロセス単体への kill も実施 (非 Unix のフォールバック兼念押し)。
    // 既に終了していれば kill はエラーになるが無視する。
    let _ = child.kill();
    let reap_started = Instant::now();
    loop {
        match child.try_wait() {
            // 回収完了 (もしくは既に回収済み)。
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {
                if reap_started.elapsed() >= YTDLP_KILL_REAP_TIMEOUT {
                    break;
                }
                std::thread::sleep(YTDLP_POLL_INTERVAL);
            }
        }
    }
}

/// reader thread を **bounded** に join する (指摘1(b))。
///
/// `JoinHandle::join` は素では無期限ブロックする。group kill で子孫まで殺せば
/// pipe は EOF になり reader はすぐ終了するが、万一 FD が残ったケースでも
/// 呼び出しが再ハングしないよう、`is_finished` を [`YTDLP_READER_JOIN_TIMEOUT`]
/// まで poll し、終了済みなら join、未終了なら detach する。detach した thread は
/// FD が閉じれば自然終了し、プロセス終了でいずれ回収される (リークしても応答性は
/// 損なわない)。
pub(crate) fn bounded_join(handle: std::thread::JoinHandle<()>) {
    let started = Instant::now();
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        if started.elapsed() >= YTDLP_READER_JOIN_TIMEOUT {
            // 猶予超過: join せず detach (再ハング回避)。
            return;
        }
        std::thread::sleep(YTDLP_POLL_INTERVAL);
    }
}

/// stderr reader thread (`JoinHandle<String>`) を bounded に join し、収集済み
/// stderr を返す。猶予超過で諦めた場合は空文字を返す (種別分類は `Other` へ縮退)。
fn bounded_join_string(handle: std::thread::JoinHandle<String>) -> String {
    let started = Instant::now();
    loop {
        if handle.is_finished() {
            return handle.join().unwrap_or_default();
        }
        if started.elapsed() >= YTDLP_READER_JOIN_TIMEOUT {
            return String::new();
        }
        std::thread::sleep(YTDLP_POLL_INTERVAL);
    }
}

/// deadline 超過時の固定エラー文言。
///
/// secret / cookie / token / URL 生値を一切含まない固定文言にすることで、
/// 既存 redaction 方針 (F1) を満たす。
fn deadline_error() -> YtDlpError {
    YtDlpError::new(
        YtDlpErrorKind::Other,
        "ダウンロードが制限時間内に完了しませんでした。時間をおいて再試行してください。",
    )
}

/// キャンセルトークン (`Cancel`) が立っているか (指摘2)。`None` なら常に false。
pub(crate) fn is_cancelled(cancel: Cancel<'_>) -> bool {
    cancel
        .map(|flag| flag.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// キャンセルトークン経路で中断した時の固定エラー (`Other` 種別)。
///
/// GUI 側は `cancel_flag` を見て `ConvertError::cancelled()` へ写像するため、
/// 種別は progress-Err 経路と同じく `Other` でよい。文言は core 共通の
/// [`crate::CANCEL_MESSAGE`] と揃える。secret は含まない。
fn cancel_error() -> YtDlpError {
    YtDlpError::new(YtDlpErrorKind::Other, crate::CANCEL_MESSAGE)
}

/// yt-dlp の `--newline` 進捗行から download パーセントを取り出す。
///
/// 例: `[download]  42.3% of 10.00MiB at 1.00MiB/s ETA 00:05`
fn parse_ytdlp_download_percent(line: &str) -> Option<f64> {
    let rest = line.trim_start().strip_prefix("[download]")?;
    let rest = rest.trim_start();
    let percent_token = rest.split_whitespace().next()?;
    let number = percent_token.strip_suffix('%')?;
    let value = number.parse::<f64>().ok()?;
    // A15: NaN / inf / 負を素通しさせない。後段の `(percent * 0.9).round() as u8` は
    // NaN/inf/負で未定義・異常値になり得るため、有限かつ非負のみ受理する。
    if value.is_finite() && value >= 0.0 {
        Some(value)
    } else {
        None
    }
}

/// A1: yt-dlp の DL 前 (解決 / 抽出) フェーズの行から、進捗パーセント (1-10%) を
/// 推定する。`[download] %` 行はここでは扱わない (別途 `parse_ytdlp_download_percent`)。
///
/// yt-dlp は本体 URL 解決中に `[<extractor>] Extracting URL: ...` /
/// `Downloading webpage` / `Downloading m3u8 information` / `[info] ...` 等を出す。
/// これらを段階的な 1-10% にマップし、「解決中」で 0% のまま固まって見えるのを防ぐ。
/// 該当しない行は `None`。
fn parse_ytdlp_resolve_percent(line: &str) -> Option<u8> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("extracting url") {
        Some(2)
    } else if lower.contains("downloading webpage") {
        Some(4)
    } else if lower.contains("downloading m3u8") || lower.contains(".m3u8") {
        Some(6)
    } else if lower.contains("downloading") && lower.contains("information") {
        // `Downloading API JSON` / `Downloading player ...` 等のメタ取得段。
        Some(8)
    } else if lower.starts_with("[info]") || lower.contains("[info] ") {
        Some(10)
    } else {
        None
    }
}

/// yt-dlp の stderr からエラー種別を分類する。
///
/// Slice 5 (認証 auth helper) が `AuthRequired` を機械的に拾えるよう、認証起因の
/// メッセージを優先的に判定する。
///
/// ## 秘密漏洩防止 (F1)
/// yt-dlp は `--cookies <temp-path>` 付きで起動するため、stderr に cookie 一時
/// ファイルのパスを出すことがある。**種別判定 (auth/unsupported/other) には raw
/// stderr を使ってよい**が、`Other` 種別で stderr 末尾をユーザ向け文言へ添える
/// 際は、cookie file の絶対パス・親ディレクトリ・ファイル名 (stem 含む) を
/// `<redacted>` に伏せてから返す。これにより CLI stderr / 将来の MCP 応答 / ログ
/// へ cookie 一時パスが流出しない。
fn classify_ytdlp_failure(stderr: &str, cookie_file: Option<&Path>) -> YtDlpError {
    let lower = stderr.to_ascii_lowercase();

    // 修正 (Slice H / reviewer fix): 分類順序を厳密にする。複合 stderr
    // (例: `HTTP Error 403 ... No video formats found` や
    // `Sign in to confirm your age ... No video formats found`) が真の認証でも
    // ExtractionBlocked に落ちると login window / auth helper が開かず private 動画が
    // 変換不能のまま放置される。そこで:
    //   1. bot 確認  → ExtractionBlocked (最優先・非auth。ログインでは解決しない)。
    //   2. 真の認証  → AuthRequired。
    //   3. no formats → ExtractionBlocked (ただし上記 auth に当たらない時のみ)。
    //   4. それ以外  → Unsupported / Other。

    // 1. ボット確認: YouTube のボット確認は個別ログイン不要 (deno/node 等の JS
    //    ランタイムや時間をおいた再試行で解消し得る)。最優先で非 auth に確定する。
    let is_bot_confirmation = lower.contains("confirm you're not a bot")
    || lower.contains("confirm you’re not a bot") // 全角アポストロフィ (curly) 防御
    || lower.contains("not a bot");

    if is_bot_confirmation {
        return YtDlpError::new(
            YtDlpErrorKind::ExtractionBlocked,
            "公開動画のようですが、配信側 (YouTube 等) に取得をブロックされた可能性が \
       あります。deno / node の導入、または時間をおいての再試行をお試しください。",
        );
    }

    // 2. 真の認証要求のみ AuthRequired にする。ボット確認 (上で確定済み) を含む広い
    //    「sign in」一致は使わず、文脈で限定する。
    // - 年齢制限 (`Sign in to confirm your age`) はログインで解決し得るので auth。
    // - `Sign in if you've been granted access` (限定公開) も auth。
    // - 403 forbidden / private は真の権限不足なので auth。
    // - `Sign in to view` / `This video is private` / `Join this channel to get
    //   access` (メンバー限定) も真の認証起因なので auth。
    let is_auth = lower.contains("http error 401")
        || lower.contains("http error 403")
        || lower.contains(" 401 ")
        || lower.contains(" 403 ")
        || lower.contains("login required")
        || lower.contains("requires login")
        || lower.contains("sign in if you")
        || lower.contains("sign in to confirm your age")
        || lower.contains("sign in to view")
        || lower.contains("please sign in")
        || lower.contains("please log in")
        || lower.contains("private video")
        || lower.contains("this video is private")
        || lower.contains("join this channel to get access")
        || lower.contains("members-only")
        || lower.contains("membership")
        || lower.contains("this video is only available");

    if is_auth {
        return YtDlpError::new(
            YtDlpErrorKind::AuthRequired,
            "認証が必要です。ログイン情報 (cookie) を取得してから再試行してください。",
        );
    }

    // 3. 「No video formats found」: 抽出失敗。多くはボット確認 / JS チャレンジ
    //    未解決が原因で、ログインでは直らない。ただし上記 auth マーカーに当たらない
    //    場合のみ ExtractionBlocked に確定する (複合 stderr で auth が勝つよう、
    //    auth 判定の後ろに置き `!is_auth` を担保する)。
    if lower.contains("no video formats found") {
        return YtDlpError::new(
            YtDlpErrorKind::ExtractionBlocked,
            "公開動画のようですが、配信側 (YouTube 等) に取得をブロックされた可能性が \
       あります。deno / node の導入、または時間をおいての再試行をお試しください。",
        );
    }

    let is_unsupported = lower.contains("unsupported url")
        || lower.contains("unable to extract")
        || lower.contains("no media found");

    if is_unsupported {
        return YtDlpError::new(
            YtDlpErrorKind::Unsupported,
            "この URL からは動画を解決できませんでした。",
        );
    }

    // それ以外: stderr の末尾を控えめに添える。ただし cookie 一時パスを redact
    // してから添える (F1)。
    let tail = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    let tail = redact_secrets(&tail, cookie_file);
    let message = if tail.is_empty() {
        "yt-dlp の実行に失敗しました。".to_string()
    } else {
        format!("yt-dlp の実行に失敗しました: {tail}")
    };
    YtDlpError::new(YtDlpErrorKind::Other, message)
}

/// ユーザ向けエラー文言から秘密 (cookie 一時ファイルのパス・親ディレクトリ・
/// ファイル名、URL クエリトークン) を `<redacted>` へ伏せる (F1)。
///
/// cookie file パスは `--cookies <temp-path>` 経由で yt-dlp に渡るため、yt-dlp が
/// stderr へ出すと文言に混入し得る。絶対パスそのものだけでなく、ファイル名
/// (stem 含む) や親ディレクトリ名も伏せる (どれか 1 つでも残ると一時ファイル名が
/// 特定され得るため)。
fn redact_secrets(text: &str, cookie_file: Option<&Path>) -> String {
    let mut out = text.to_string();

    if let Some(path) = cookie_file {
        // 伏せる候補トークンを長い順に並べる (部分一致の取りこぼし回避)。
        let mut tokens: Vec<String> = Vec::new();
        // 絶対パス全体。
        tokens.push(path.to_string_lossy().to_string());
        // 親ディレクトリの絶対パス + ディレクトリ名。
        if let Some(parent) = path.parent() {
            tokens.push(parent.to_string_lossy().to_string());
            if let Some(dir_name) = parent.file_name() {
                tokens.push(dir_name.to_string_lossy().to_string());
            }
        }
        // ファイル名 (拡張子あり) と stem (拡張子なし)。
        if let Some(file_name) = path.file_name() {
            tokens.push(file_name.to_string_lossy().to_string());
        }
        if let Some(stem) = path.file_stem() {
            tokens.push(stem.to_string_lossy().to_string());
        }
        // 長いトークンから順に置換し、空文字や極端に短い汎用語は除外する。
        tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
        for token in tokens {
            if token.len() >= 4 && out.contains(&token) {
                out = out.replace(&token, "<redacted>");
            }
        }
    }

    // URL のクエリ文字列 (`?...`) を伏せる: token/sig 等が載り得るため
    // (Slice 2 レビューの URL redaction 申し送り)。空白区切りトークン単位で処理。
    out = redact_url_queries(&out);

    out
}

/// 空白区切りトークンのうち、`http(s)://` を含み `?` を持つものはクエリ部
/// (`?...`) と fragment (`#...`) を `?<redacted>` へ伏せる。
///
/// 署名付き / 期限付き URL の `?token=...&sig=...` 等が CLI stderr / MCP の
/// JSON-RPC 応答 / ログに生で混入するのを防ぐための共通 sanitizer。core に集約し
/// CLI / MCP の両境界から再利用する (変換ロジックは core が single source of
/// truth)。**実際のダウンロードに渡す URL は生のまま使うこと** — この関数は
/// 「ログ / 応答に出す文字列」だけに適用する。
pub fn redact_url_queries(text: &str) -> String {
    text.split(' ')
        .map(|tok| {
            let looks_url = tok.contains("http://") || tok.contains("https://");
            if looks_url {
                // query (`?`) と fragment (`#`) のいずれか早い方以降を伏せる。
                // 署名 URL は `?token=...&sig=...` 形式が主だが、fragment に載るケースも
                // あるため両方を防御する。
                let cut = match (tok.find('?'), tok.find('#')) {
                    (Some(q), Some(h)) => Some(q.min(h)),
                    (Some(q), None) => Some(q),
                    (None, Some(h)) => Some(h),
                    (None, None) => None,
                };
                if let Some(idx) = cut {
                    return format!("{}?<redacted>", &tok[..idx]);
                }
            }
            tok.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// yt-dlp 経路の生成物を隔離する job folder `<output_dir>/<stem>-mp3-<ts>/` を作る
/// (A4/A5/A7)。
///
/// ## なぜ専用 folder か
/// yt-dlp の出力テンプレートは `%(title)s` を使うため DL 前にファイル名が確定しない。
/// 従来は出力 dir 直下に生 MP3 を出し `resolve_output_mp3(output_dir)` で「最新 mp3」を
/// 拾っていたが、(a) 出力先に生 MP3 が散らかる (A4)、(b) 同一 dir に既存 mp3 があると
/// 誤検出 / 並列実行で取り違える (A7)、という問題があった。
///
/// job folder に隔離すれば、解決は job folder 内だけを見れば一意に決まり、分割時の
/// 中間生 MP3 もこの folder 内で完結して掃除しやすい (A5)。
///
/// title が DL 前に不明なので folder 名 stem は固定 `download` とし、他経路
/// (`create_output_folder`) と同じ `<stem>-mp3-<timestamp>` 命名規則・タイムスタンプ
/// 粒度 (秒) に揃える。秒粒度衝突を避けるため共通ヘルパ [`create_exclusive_dir`]
/// (`fs::create_dir` による排他作成 + サフィックスリトライ) を使い、`create_dir_all`
/// では candidate を作らない。これにより呼び出し側でアームされるガードが先行ジョブの
/// 成果物を `remove_dir_all` してしまう退行を防ぐ。
fn create_ytdlp_job_folder(output_dir: &Path) -> Result<PathBuf, String> {
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    create_exclusive_dir(output_dir, &format!("download-mp3-{timestamp}"))
}

/// DL 完了後、固定名 `download-mp3-<ts>` の job folder を動画タイトルベースの名前
/// (`<safe_title>-mp3-<ts>`) にリネームする (best-effort)。
///
/// `create_ytdlp_job_folder` は DL 前にタイトルが不明なため固定 stem `download` で
/// job folder を作るが、DL 後は `mp3_path`(yt-dlp が `%(title)s` テンプレートで
/// 生成したファイル名) からタイトルが分かる。ユーザーが後から Finder / Downloads で
/// 見分けやすいよう、既存フォルダ内のタイムスタンプ部分は保ったままリネームする。
///
/// リネームに失敗しても (権限・同時実行での衝突等) 変換自体は成功しているため、
/// 元の job_folder をそのまま返す (エラーにしない)。
fn rename_job_folder_to_title(job_folder: &Path, mp3_path: &Path) -> PathBuf {
    let Some(parent) = job_folder.parent() else {
        return job_folder.to_path_buf();
    };
    let stem = mp3_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("audio");
    let safe_stem = sanitize_name(stem);

    // 既存 job folder 名 `download-mp3-<ts>` からタイムスタンプ部分を引き継ぐ。
    // 取れない場合はタイムスタンプなしのタイトルのみで代替する。
    let timestamp_suffix = job_folder
        .file_name()
        .and_then(|value| value.to_str())
        .and_then(|name| name.strip_prefix("download-mp3-"))
        .filter(|value| !value.is_empty());
    let new_base_name = match timestamp_suffix {
        Some(timestamp) => format!("{safe_stem}-mp3-{timestamp}"),
        None => safe_stem,
    };

    if job_folder.file_name().and_then(|value| value.to_str()) == Some(new_base_name.as_str()) {
        return job_folder.to_path_buf();
    }

    for suffix in 0..1000 {
        let candidate = if suffix == 0 {
            parent.join(&new_base_name)
        } else {
            parent.join(format!("{new_base_name}-{suffix}"))
        };
        match fs::rename(job_folder, &candidate) {
            Ok(()) => return candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return job_folder.to_path_buf(),
        }
    }
    job_folder.to_path_buf()
}

/// `output_dir` 直下から最も新しい .mp3 を解決する。
fn resolve_output_audio(output_dir: &Path) -> Result<String, YtDlpError> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

    let entries = fs::read_dir(output_dir).map_err(|error| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            format!("出力フォルダを読み取れませんでした: {error}"),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            YtDlpError::new(
                YtDlpErrorKind::Other,
                format!("出力ファイルの取得に失敗しました: {error}"),
            )
        })?;
        let path = entry.path();
        if !matches!(
            path.extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("mp3" | "m4a" | "aac" | "opus" | "ogg" | "webm" | "wav" | "flac")
        ) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let replace = match &newest {
            Some((current, _)) => modified >= *current,
            None => true,
        };
        if replace {
            newest = Some((modified, path));
        }
    }

    newest
        .map(|(_, path)| path.to_string_lossy().to_string())
        .ok_or_else(|| {
            YtDlpError::new(
                YtDlpErrorKind::Other,
                "yt-dlp は成功しましたが音声出力が見つかりませんでした。",
            )
        })
}

fn optimize_audio_to_transcription_mp3<R: BinaryResolver + ?Sized>(
    resolver: &R,
    input: &Path,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
) -> Result<String, YtDlpError> {
    optimize_audio_to_transcription_mp3_with_deadline(
        resolver,
        input,
        progress,
        cancel,
        YTDLP_PROCESS_DEADLINE,
    )
}

fn optimize_audio_to_transcription_mp3_with_deadline<R: BinaryResolver + ?Sized>(
    resolver: &R,
    input: &Path,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
    deadline: Duration,
) -> Result<String, YtDlpError> {
    let ffmpeg = resolver
        .resolve("ffmpeg")
        .map_err(|message| YtDlpError::new(YtDlpErrorKind::BinaryNotFound, message))?;
    let parent = input.parent().ok_or_else(|| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            "最適化対象の保存先を解決できませんでした。",
        )
    })?;
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("audio");
    let output = parent.join(format!("{stem}.mp3"));
    let temporary = parent.join(format!(".{stem}.optimizing.mp3"));
    let _ = fs::remove_file(&temporary);
    let args = vec![
        OsString::from("-y"),
        OsString::from("-nostdin"),
        OsString::from("-i"),
        input.as_os_str().into(),
        OsString::from("-vn"),
        OsString::from("-map_metadata"),
        OsString::from("-1"),
        OsString::from("-c:a"),
        OsString::from("libmp3lame"),
        OsString::from("-b:a"),
        OsString::from(OPTIMIZED_MP3_BITRATE),
        OsString::from("-ar"),
        OsString::from(OPTIMIZED_MP3_SAMPLE_RATE),
        OsString::from("-ac"),
        OsString::from("1"),
        OsString::from("-threads"),
        OsString::from("1"),
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        temporary.as_os_str().into(),
    ];
    if let Err(message) = crate::run_ffmpeg_with_cancel_with_deadline(
        progress,
        &ffmpeg,
        args,
        0.0,
        "最適化中",
        "文字起こし向けに低容量 MP3 を生成しています。",
        cancel,
        deadline,
    ) {
        let _ = fs::remove_file(&temporary);
        return Err(YtDlpError::new(YtDlpErrorKind::Other, message));
    }
    fs::rename(&temporary, &output).map_err(|error| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            format!("最適化済み MP3 を確定できませんでした: {error}"),
        )
    })?;
    if input != output {
        let _ = fs::remove_file(input);
    }
    Ok(output.to_string_lossy().to_string())
}

/// URL が「直メディア (ffmpeg で直接扱える)」か「要 extractor (yt-dlp 経路)」かを
/// 判定する。
///
/// - `Direct`: 既存の [`crate::download_media`] / HLS 経路で処理できる直リンク
///   (`.mp4` / `.m3u8` 等の拡張子を持つ)。
/// - `Extractor`: ページ / 埋め込み URL。yt-dlp で本体を解決する必要がある。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaRoute {
    /// 直メディア。既存 download_media 経路。
    Direct,
    /// 要 extractor。yt-dlp 経路。
    Extractor,
}

/// URL が直メディアを指すか判定する。
///
/// パス末尾セグメントの拡張子が既知メディア拡張子 (`mp4` / `m3u8` 等) なら直。
pub fn is_direct_media(url: &Url) -> bool {
    // HLS は直として扱う (既存 download_hls 経路がある)。
    if crate::is_hls_url(url) {
        return true;
    }
    let ext = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|value| !value.is_empty()))
        .and_then(|name| {
            Path::new(name)
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase())
        });
    matches!(
        ext.as_deref(),
        Some("mp4" | "mov" | "m4v" | "webm" | "mp3" | "m4a" | "ts" | "aac")
    )
}

/// URL を直メディア経路 / yt-dlp 経路へ振り分ける。
pub fn route_for_url(url: &Url) -> MediaRoute {
    if is_direct_media(url) {
        MediaRoute::Direct
    } else {
        MediaRoute::Extractor
    }
}

/// URL (要 extractor) -> MP3 を yt-dlp 経路で行う高水準エントリ。
///
/// GUI (host) / MCP / CLI のいずれからも同一ロジックで呼べるよう、
/// 「yt-dlp で本体を解決 -> 分割時は `segment_mp3_copy`、非分割時は metadata から
/// [`ConversionSummary`] を構築」までを core に集約する。MCP 側の
/// `extract_via_ytdlp_to_summary` と挙動を一致させること (重複は follow-up で DRY 化)。
///
/// この関数は **直リンク判定を行わない** (呼び出し側で [`route_for_url`] /
/// [`is_direct_media`] により Extractor と確定してから呼ぶ前提)。直リンクは
/// 既存 [`crate::download_media`] 経路へ振り分けること。
///
/// - `cookie`: `Cookie:` ヘッダ文字列。`Some` なら Netscape jar に変換し
///   `--cookies` で渡す (argv に値は載らない)。空 / 空白は無視。
/// - `referer`: `--referer`。`None` なら対象 URL のオリジンを補完。
/// - `split_minutes`: `Some(n>0)` で n 分ごとに `-c copy` 分割。`None` なら単一 MP3。
/// - `optimize`: `true` でファイルサイズ優先 (`--audio-quality 3`)。
///
/// 戻り値は種別付き [`YtDlpError`]。呼び出し側は `kind()` を見て
/// `AuthRequired` (要認証) / `ExtractionBlocked` (ボット確認・取得不可) を出し
/// 分けられる。yt-dlp 後段の分割 / metadata 取得失敗は `Other` 種別へ包む。
#[allow(clippy::too_many_arguments)]
pub fn extract_url_to_mp3<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: &str,
    output_dir: &Path,
    split_minutes: Option<u64>,
    optimize: bool,
    cookie: Option<&str>,
    referer: Option<&str>,
    progress: ProgressFn<'_>,
) -> Result<ConversionSummary, YtDlpError> {
    // 後方互換: cancel トークン無し (None) で cancel 対応版へ委譲する。MCP / CLI の
    // 既存呼び出しはこの公開 API を使うため挙動不変 (キャンセルチェックを行わない)。
    extract_url_to_mp3_with_cancel(
        resolver,
        url,
        output_dir,
        split_minutes,
        optimize,
        cookie,
        referer,
        progress,
        None,
    )
}

/// [`extract_url_to_mp3`] の協調的キャンセル対応版 (Slice I-1 reviewer fix / 指摘2)。
///
/// `cancel` に `Some(flag)` を渡すと、yt-dlp 進捗ポーリングループと、分割時の
/// ffmpeg (`segment_mp3_copy`) 行読みループが各反復でフラグを確認し、立っていれば
/// child (プロセスグループ) を kill して即座に中断エラーを返す。`None` なら
/// [`extract_url_to_mp3`] と完全に同一挙動。
///
/// GUI (`desktop-host`) は `CancelFlag` の `Arc<AtomicBool>` を `Some(&flag)` で渡す。
#[allow(clippy::too_many_arguments)]
pub fn extract_url_to_mp3_with_cancel<R: BinaryResolver + ?Sized>(
    resolver: &R,
    url: &str,
    output_dir: &Path,
    split_minutes: Option<u64>,
    optimize: bool,
    cookie: Option<&str>,
    referer: Option<&str>,
    progress: ProgressFn<'_>,
    cancel: Cancel<'_>,
) -> Result<ConversionSummary, YtDlpError> {
    let options = YtDlpOptions {
        optimize_for_size: optimize,
        user_agent: None,
    };

    let mp3 = extract_audio_via_ytdlp_with_deadline_cancel(
        resolver,
        url,
        output_dir,
        cookie,
        referer,
        &options,
        progress,
        YTDLP_PROCESS_DEADLINE,
        cancel,
    )?;

    let mp3 = if optimize {
        match optimize_audio_to_transcription_mp3(resolver, Path::new(&mp3), progress, cancel) {
            Ok(path) => path,
            Err(error) => {
                if let Some(folder) = Path::new(&mp3).parent() {
                    let _ = fs::remove_dir_all(folder);
                }
                return Err(error);
            }
        }
    } else {
        mp3
    };

    let mp3_path = PathBuf::from(&mp3);
    // A4/A7: yt-dlp は専用 job folder 内に MP3 を生成済み。その folder が最終成果物の
    // 出力フォルダ (`output_folder`) になる。
    let job_folder = mp3_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| output_dir.to_path_buf());

    if let Some(split_minutes) = split_minutes {
        // MCP `extract_via_ytdlp_to_summary` と同じく、二重エンコードを避けるため
        // `convert_media_blocking` (libmp3lame 再エンコード) ではなく `segment_mp3_copy`
        // (`-c copy` stream copy) で分割する。正規化は yt-dlp 側で担保済み。
        // 指摘2: 分割 (ffmpeg `-c copy`) ループにも cancel トークンを伝播させる。
        //
        // A5: segment_mp3_copy は出力 dir 配下に **別の** job folder を作りそこへ分割
        // セグメントを出す。yt-dlp が生成した中間生 MP3 (= job_folder 内) はもう不要なので、
        // 分割の成否に関わらず job_folder ごと削除して残置を消す。
        let summary = crate::segment_mp3_copy_with_cancel(
            resolver,
            &mp3_path,
            output_dir,
            split_minutes,
            progress,
            cancel,
        )
        .map_err(|message| YtDlpError::new(YtDlpErrorKind::Other, message));
        // 中間生 MP3 を含む yt-dlp job folder を掃除する (best-effort)。
        let _ = fs::remove_dir_all(&job_folder);
        return summary;
    }

    let metadata = std::fs::metadata(&mp3_path).map_err(|error| {
        YtDlpError::new(
            YtDlpErrorKind::Other,
            format!("出力ファイルの情報を取得できませんでした: {error}"),
        )
    })?;

    // job folder を固定名 `download-mp3-<ts>` から動画タイトルベースの名前へ
    // リネームする (best-effort)。失敗しても変換自体は成功扱いのまま続行する。
    let final_folder = rename_job_folder_to_title(&job_folder, &mp3_path);
    let final_mp3_path = if final_folder == job_folder {
        mp3
    } else {
        let file_name = mp3_path.file_name().unwrap_or_default();
        final_folder.join(file_name).to_string_lossy().to_string()
    };

    Ok(ConversionSummary {
        // A4: 出力フォルダは job folder (生 MP3 を内包) を指す。出力 dir 直下に生 MP3 を
        // 出さないので「フォルダと生 MP3 が両方生まれる」問題が解消する。
        output_folder: final_folder.to_string_lossy().to_string(),
        file_count: 1,
        total_size_bytes: metadata.len(),
        files: vec![FileSummary {
            path: final_mp3_path,
            size_bytes: metadata.len(),
        }],
        duration_seconds: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> Url {
        Url::parse(url).unwrap()
    }

    /// FIX-B 退行回帰: `create_ytdlp_job_folder` は候補同名が既存でも再利用せず、
    /// サフィックス付き新規・排他フォルダを返し、既存フォルダ内容を温存する。
    /// 内部タイムスタンプは秒粒度のため、まず1回作って実フォルダ名を得てから、同名を
    /// 事前作成しておくと2回目以降が `AlreadyExists` → サフィックスリトライ経路を踏む。
    /// `create_dir_all` 退行だと既存を再利用し、アーム済みガードが先行成果物を消す。
    #[test]
    fn create_ytdlp_job_folder_skips_preexisting_and_preserves_contents() {
        let base = std::env::temp_dir().join(format!(
            "revmagic-ytdlp-jobfolder-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        fs::create_dir_all(&base).unwrap();

        // 1回目: 実際のタイムスタンプ命名のフォルダを作る。
        let first = create_ytdlp_job_folder(&base).unwrap();
        let marker = first.join("first-job-output.mp3");
        fs::write(&marker, b"keep this").unwrap();

        // 2回目: 同一秒なら first と同名候補で衝突し、別秒でも新規フォルダになる。
        // いずれにせよ first を再利用してはならない。
        let second = create_ytdlp_job_folder(&base).unwrap();

        assert_ne!(first, second, "must not reuse the first job folder");
        assert!(second.is_dir(), "second folder must exist");
        assert_eq!(
            fs::read_dir(&second).unwrap().count(),
            0,
            "second folder must be empty (freshly created)"
        );
        assert!(
            marker.exists(),
            "first job's output must survive (no false reuse)"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cookie_header_to_netscape_emits_tab_separated_rows() {
        let target = parse("https://www.loom.com/share/abc");
        let netscape = cookie_header_to_netscape("a=1; sid=xyz; empty=", &target);

        assert!(netscape.starts_with("# Netscape HTTP Cookie File\n"));
        // domain は先頭ドット付き host、include_subdomains TRUE、path /、secure TRUE。
        assert!(netscape.contains(".www.loom.com\tTRUE\t/\tTRUE\t0\ta\t1\n"));
        assert!(netscape.contains(".www.loom.com\tTRUE\t/\tTRUE\t0\tsid\txyz\n"));
        // 値が空でも name があれば 1 行出す。
        assert!(netscape.contains(".www.loom.com\tTRUE\t/\tTRUE\t0\tempty\t\n"));
        // 各データ行は 7 フィールド (TAB x6)。
        for line in netscape
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            assert_eq!(
                line.matches('\t').count(),
                6,
                "row must have 7 fields: {line}"
            );
        }
    }

    #[test]
    fn cookie_header_to_netscape_http_is_insecure() {
        let target = parse("http://example.com/page");
        let netscape = cookie_header_to_netscape("k=v", &target);
        assert!(netscape.contains(".example.com\tTRUE\t/\tFALSE\t0\tk\tv\n"));
    }

    #[test]
    fn cookie_header_skips_malformed_pairs() {
        let target = parse("https://e.com/");
        let netscape = cookie_header_to_netscape("good=1; noequals; =novalue;  ; b=2", &target);
        let data_rows: Vec<&str> = netscape
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        // good=1 と b=2 の 2 行のみ (noequals / =novalue / 空白はスキップ)。
        assert_eq!(data_rows.len(), 2);
        assert!(netscape.contains("\tgood\t1\n"));
        assert!(netscape.contains("\tb\t2\n"));
    }

    #[test]
    fn parse_download_percent_extracts_value() {
        assert_eq!(
            parse_ytdlp_download_percent("[download]  42.3% of 10.00MiB at 1.00MiB/s ETA 00:05"),
            Some(42.3)
        );
        assert_eq!(
            parse_ytdlp_download_percent("[download] 100% of 1.00MiB"),
            Some(100.0)
        );
        assert_eq!(parse_ytdlp_download_percent("[info] something"), None);
        assert_eq!(
            parse_ytdlp_download_percent("[download] Destination: foo.mp4"),
            None
        );
    }

    // A15: NaN / inf / 負パーセントを素通しさせない。
    #[test]
    fn parse_download_percent_rejects_nonfinite_and_negative() {
        assert_eq!(
            parse_ytdlp_download_percent("[download] NaN% of 1MiB"),
            None
        );
        assert_eq!(
            parse_ytdlp_download_percent("[download] inf% of 1MiB"),
            None
        );
        assert_eq!(parse_ytdlp_download_percent("[download] -5% of 1MiB"), None);
        // 通常値は引き続き通る。
        assert_eq!(
            parse_ytdlp_download_percent("[download] 0% of 1MiB"),
            Some(0.0)
        );
    }

    // A1: DL 前フェーズ行を 1-10% にマップする。
    #[test]
    fn parse_resolve_percent_maps_early_phases() {
        assert_eq!(
            parse_ytdlp_resolve_percent("[loom] Extracting URL: https://x"),
            Some(2)
        );
        assert_eq!(
            parse_ytdlp_resolve_percent("[loom] abc: Downloading webpage"),
            Some(4)
        );
        assert_eq!(
            parse_ytdlp_resolve_percent("[hlsnative] Downloading m3u8 information"),
            Some(6)
        );
        assert_eq!(
            parse_ytdlp_resolve_percent("[info] Available formats"),
            Some(10)
        );
        // DL 進捗行 / 無関係行はここでは扱わない。
        assert_eq!(parse_ytdlp_resolve_percent("[download] 42% of 1MiB"), None);
        assert_eq!(parse_ytdlp_resolve_percent("random noise"), None);
    }

    // A10: 制御文字 (TAB/CR/LF) を含む cookie pair を skip し、行注入を防ぐ。
    #[test]
    fn cookie_header_skips_control_char_injection() {
        let target = parse("https://e.com/");
        // value に改行 + 注入行、name に TAB を仕込む。
        let netscape =
            cookie_header_to_netscape("good=1; evil=a\nINJECTED\tROW; ta\tb=2; b=ok", &target);
        // 注入を試みた pair (制御文字を含む) は出力されない。
        assert!(
            !netscape.contains("INJECTED"),
            "control-char injection leaked: {netscape}"
        );
        // 正常な pair は残る。
        assert!(netscape.contains("\tgood\t1\n"));
        assert!(netscape.contains("\tb\tok\n"));
        // 各データ行は厳密に 7 フィールド (TAB x6)。注入で 6 を超える行が無い。
        for line in netscape
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            assert_eq!(
                line.matches('\t').count(),
                6,
                "row must have 7 fields: {line:?}"
            );
        }
    }

    #[test]
    fn has_control_char_detects_tab_cr_lf() {
        assert!(has_control_char("a\tb"));
        assert!(has_control_char("a\nb"));
        assert!(has_control_char("a\rb"));
        assert!(has_control_char("a\0b"));
        assert!(!has_control_char("normal-value_123"));
    }

    #[test]
    fn classify_failure_detects_auth() {
        for stderr in [
            "ERROR: HTTP Error 403: Forbidden",
            "ERROR: unable to download: HTTP Error 401",
            "ERROR: This video requires login",
            "Please sign in to view",
            "ERROR: Private video. Sign in if you've been granted access",
        ] {
            let err = classify_ytdlp_failure(stderr, None);
            assert_eq!(err.kind(), &YtDlpErrorKind::AuthRequired, "stderr={stderr}");
            assert!(err.is_auth_error());
        }
    }

    #[test]
    fn classify_failure_detects_unsupported() {
        let err = classify_ytdlp_failure("ERROR: Unsupported URL: https://example.com/page", None);
        assert_eq!(err.kind(), &YtDlpErrorKind::Unsupported);
        assert!(!err.is_auth_error());
    }

    #[test]
    fn classify_failure_other_keeps_tail() {
        let err = classify_ytdlp_failure("line one\nERROR: some network blip\n", None);
        assert_eq!(err.kind(), &YtDlpErrorKind::Other);
        assert!(err.to_string().contains("some network blip"));
    }

    #[test]
    fn classify_failure_other_redacts_cookie_path() {
        // F1: yt-dlp が cookie file パスを stderr に出すケース。
        let cookie = PathBuf::from("/var/folders/xy/revmagic-cookies/cookies-20260616-abc123.txt");
        let stderr = format!(
            "ERROR: unable to load cookies from {} due to parse error",
            cookie.display()
        );
        let err = classify_ytdlp_failure(&stderr, Some(cookie.as_path()));
        let message = err.to_string();
        assert_eq!(err.kind(), &YtDlpErrorKind::Other);
        // cookie file の絶対パス・親ディレクトリ名・ファイル名・stem いずれも残らない。
        assert!(
            !message.contains("cookies-20260616-abc123"),
            "stem leaked: {message}"
        );
        assert!(
            !message.contains("cookies-20260616-abc123.txt"),
            "file name leaked: {message}"
        );
        assert!(
            !message.contains("revmagic-cookies"),
            "dir name leaked: {message}"
        );
        assert!(
            !message.contains("/var/folders/xy"),
            "abs path leaked: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "should contain redaction marker: {message}"
        );
    }

    #[test]
    fn classify_failure_auth_message_never_includes_cookie_path() {
        // auth 判定でも、固定文言なので cookie path は元から載らない。
        let cookie = PathBuf::from("/tmp/revmagic-cookies/cookies-secretstem.txt");
        let stderr = format!("ERROR: HTTP Error 403 reading {}", cookie.display());
        let err = classify_ytdlp_failure(&stderr, Some(cookie.as_path()));
        assert_eq!(err.kind(), &YtDlpErrorKind::AuthRequired);
        let message = err.to_string();
        assert!(!message.contains("secretstem"));
        assert!(!message.contains("revmagic-cookies"));
    }

    #[test]
    fn redact_url_queries_hides_query_tokens() {
        let input = "failed at https://cdn.example.com/seg.ts?token=SECRETSIG&exp=1 retry";
        let out = redact_url_queries(input);
        assert!(!out.contains("SECRETSIG"), "query token leaked: {out}");
        assert!(
            out.contains("https://cdn.example.com/seg.ts?<redacted>"),
            "out: {out}"
        );
        // クエリの無い URL はそのまま。
        let plain = redact_url_queries("see https://example.com/page now");
        assert!(plain.contains("https://example.com/page"));
    }

    #[test]
    fn redact_url_queries_hides_fragment_tokens() {
        // fragment (`#`) にトークンが載るケースも伏せる。
        let input = "failed at https://example.com/path#token=SECRETFRAG retry";
        let out = redact_url_queries(input);
        assert!(!out.contains("SECRETFRAG"), "fragment token leaked: {out}");
        assert!(!out.contains("token="), "fragment key leaked: {out}");
        // URL 本体 (scheme/host/path) は保持される。
        assert!(
            out.contains("https://example.com/path?<redacted>"),
            "out: {out}"
        );

        // query と fragment が両方ある場合は早い方の区切りで cut される。
        let both = redact_url_queries("https://example.com/path?a=1#b=SECRET");
        assert!(!both.contains("SECRET"), "secret leaked: {both}");
        assert!(
            both.contains("https://example.com/path?<redacted>"),
            "both: {both}"
        );
    }

    #[test]
    fn cookie_header_filters_attributes() {
        // F4: Set-Cookie 由来の属性が混じっても cookie 名として出さない。
        let target = parse("https://www.loom.com/share/abc");
        let netscape = cookie_header_to_netscape(
            "sid=xyz; Path=/; Domain=loom.com; Expires=Wed, 21 Oct 2099 07:28:00 GMT; \
       Max-Age=3600; SameSite=Lax; Secure; HttpOnly; pref=1",
            &target,
        );
        // 実 cookie のみ残る。
        assert!(netscape.contains("\tsid\txyz\n"));
        assert!(netscape.contains("\tpref\t1\n"));
        // 属性は cookie 名として出ない (大文字小文字無視)。
        for attr in [
            "Path", "path", "Domain", "domain", "Expires", "Max-Age", "max-age", "SameSite",
            "samesite", "Secure", "secure", "HttpOnly", "httponly",
        ] {
            assert!(
                !netscape.contains(&format!("\t{attr}\t")),
                "attribute leaked as cookie name: {attr}"
            );
        }
        // データ行は sid と pref の 2 行のみ。
        let data_rows = netscape
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .count();
        assert_eq!(data_rows, 2, "only real cookies should remain");
    }

    #[test]
    fn route_direct_for_known_media() {
        assert_eq!(
            route_for_url(&parse("https://e.com/clip.mp4")),
            MediaRoute::Direct
        );
        assert_eq!(
            route_for_url(&parse("https://e.com/a/master.m3u8?t=1")),
            MediaRoute::Direct
        );
        assert_eq!(
            route_for_url(&parse("https://e.com/audio.m4a")),
            MediaRoute::Direct
        );
        assert_eq!(
            route_for_url(&parse("https://e.com/v.mov")),
            MediaRoute::Direct
        );
    }

    #[test]
    fn route_extractor_for_pages_and_embeds() {
        assert_eq!(
            route_for_url(&parse("https://www.loom.com/share/abcdef123")),
            MediaRoute::Extractor
        );
        assert_eq!(
            route_for_url(&parse("https://utage-system.com/lesson/1")),
            MediaRoute::Extractor
        );
        assert_eq!(
            route_for_url(&parse("https://e.com/watch?v=xyz")),
            MediaRoute::Extractor
        );
        assert_eq!(
            route_for_url(&parse("https://e.com/")),
            MediaRoute::Extractor
        );
    }

    #[test]
    fn build_args_includes_required_flags_and_cookie_path() {
        let cookie_path = PathBuf::from("/tmp/revmagic-cookies/cookies-x.txt");
        let template = PathBuf::from("/out/%(title)s.%(ext)s");
        let args = build_ytdlp_args(
            "UA/1.0",
            "https://ref/",
            "2",
            Some(cookie_path.as_path()),
            None,
            &template,
            "https://www.loom.com/share/abc",
        );

        let as_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // 必須フラグの隣接ペアを確認。
        assert!(window_has(&as_str, "--user-agent", "UA/1.0"));
        assert!(window_has(&as_str, "--referer", "https://ref/"));
        assert!(window_has(&as_str, "--audio-format", "mp3"));
        assert!(window_has(&as_str, "--audio-quality", "2"));
        assert!(window_has(
            &as_str,
            "--cookies",
            "/tmp/revmagic-cookies/cookies-x.txt"
        ));
        assert!(window_has(&as_str, "--output", "/out/%(title)s.%(ext)s"));
        // Slice A: 音声優先フォーマット選択と断片並列 DL。
        assert!(window_has(&as_str, "-f", "bestaudio/best"));
        assert!(window_has(&as_str, "--concurrent-fragments", "4"));
        // Slice D: ExtractAudio の単一エンコードで 44100/2ch 正規化・メタ除去を行う。
        assert!(window_has(
            &as_str,
            "--postprocessor-args",
            "ExtractAudio:-ar 44100 -ac 2 -map_metadata -1"
        ));
        assert!(as_str.iter().any(|a| a == "--extract-audio"));
        assert!(as_str.iter().any(|a| a == "--no-playlist"));
        assert!(as_str.iter().any(|a| a == "--newline"));
        // URL は最後。
        assert_eq!(
            as_str.last().map(String::as_str),
            Some("https://www.loom.com/share/abc")
        );
        // cookie 値そのものは argv に載らない (パスのみ)。
        assert!(
            !as_str.iter().any(|a| a.contains("=")),
            "no raw cookie value in argv"
        );
    }

    #[test]
    fn build_args_optimize_uses_quality_3_and_no_cookie_flag_when_absent() {
        let template = PathBuf::from("/out/%(title)s.%(ext)s");
        let args = build_ytdlp_args(
            "UA",
            "https://ref/",
            "3",
            None,
            None,
            &template,
            "https://e.com/x",
        );
        let as_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(window_has(&as_str, "--audio-quality", "3"));
        assert!(
            !as_str.iter().any(|a| a == "--cookies"),
            "no --cookies when cookie file absent"
        );
    }

    fn window_has(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn optimize_deadline_kills_silent_ffmpeg_and_cleans_temporary_output() {
        struct Resolver {
            ffmpeg: PathBuf,
        }
        impl BinaryResolver for Resolver {
            fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
                if binary == "ffmpeg" {
                    Ok(self.ffmpeg.clone())
                } else {
                    Err("unexpected binary".into())
                }
            }
        }
        let dir =
            std::env::temp_dir().join(format!("revmagic-optimize-timeout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.m4a");
        fs::write(&input, b"source").unwrap();
        let script = dir.join("ffmpeg");
        fs::write(&script, "#!/bin/sh\nsleep 600 &\nwait\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let progress = |_p: &Progress| Ok(());
        let started = Instant::now();
        let error = optimize_audio_to_transcription_mp3_with_deadline(
            &Resolver { ffmpeg: script },
            &input,
            &progress,
            None,
            Duration::from_millis(200),
        )
        .expect_err("must time out");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(error.to_string().contains("制限時間"));
        assert!(!dir.join(".source.optimizing.mp3").exists());
        assert!(input.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- 修正2 (Slice H): --ffmpeg-location 配線 ----

    #[test]
    fn build_args_includes_ffmpeg_location_when_dir_present() {
        let template = PathBuf::from("/out/%(title)s.%(ext)s");
        let ffmpeg_dir = PathBuf::from("/opt/revmagic/binaries");
        let args = build_ytdlp_args(
            "UA",
            "https://ref/",
            "2",
            None,
            Some(ffmpeg_dir.as_path()),
            &template,
            "https://e.com/x",
        );
        let as_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            window_has(&as_str, "--ffmpeg-location", "/opt/revmagic/binaries"),
            "expected --ffmpeg-location <dir>: {as_str:?}"
        );
        // URL は最後 (ffmpeg-location は URL より前に入る)。
        assert_eq!(as_str.last().map(String::as_str), Some("https://e.com/x"));
    }

    #[test]
    fn build_args_omits_ffmpeg_location_when_absent() {
        let template = PathBuf::from("/out/%(title)s.%(ext)s");
        let args = build_ytdlp_args(
            "UA",
            "https://ref/",
            "2",
            None,
            None,
            &template,
            "https://e.com/x",
        );
        let as_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !as_str.iter().any(|a| a == "--ffmpeg-location"),
            "ffmpeg-location must be absent when dir is None: {as_str:?}"
        );
    }

    // ---- 修正1 (Slice H): augmented_path ----

    #[test]
    fn augmented_path_includes_existing_real_tool_dirs_and_no_dot() {
        // 最小 PATH を base に、HOME に実在 shims dir を作って補完されることを確認。
        let home = std::env::temp_dir().join(format!(
            "revmagic-augpath-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        let mise_shims = home.join(".local/share/mise/shims");
        fs::create_dir_all(&mise_shims).unwrap();

        let base = OsString::from("/usr/bin:/bin:/usr/sbin:/sbin");
        let augmented = augmented_path(Some(base), &home);
        let parts: Vec<PathBuf> = std::env::split_paths(&augmented).collect();

        // 継承分は温存される。
        assert!(parts.iter().any(|p| p == Path::new("/usr/bin")));
        // 実在する mise shims が補完されている。
        assert!(
            parts.iter().any(|p| p == &mise_shims),
            "mise shims not augmented: {parts:?}"
        );
        // `.` / 相対 / 空エントリは含まない。
        for p in &parts {
            assert!(
                p.is_absolute(),
                "non-absolute entry in PATH: {}",
                p.display()
            );
            assert_ne!(p, Path::new("."), "`.` must not be in augmented PATH");
            assert!(!p.as_os_str().is_empty(), "empty entry must not be in PATH");
        }

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn augmented_path_skips_nonexistent_candidates_and_dedups() {
        // 何も実在しない HOME。補完候補は実在チェックで全て除外され、base のみ残る。
        let home = std::env::temp_dir().join(format!(
            "revmagic-augpath-empty-{}-{}",
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%f")
        ));
        // home 自体は作らない (.local/bin 等いずれも存在しない)。
        let base = OsString::from("/usr/bin:/usr/bin"); // 重複 base
        let augmented = augmented_path(Some(base), &home);
        let parts: Vec<PathBuf> = std::env::split_paths(&augmented).collect();
        // HOME 配下候補は実在しないので追加されない。
        assert!(
            !parts.iter().any(|p| p.starts_with(&home)),
            "nonexistent home dirs leaked: {parts:?}"
        );
    }

    // ---- 修正3 (Slice H): ボット確認 / 抽出失敗を AuthRequired から除外 ----

    #[test]
    fn classify_bot_confirmation_is_not_auth() {
        for stderr in [
      "ERROR: [youtube] xxxx: Sign in to confirm you're not a bot. Use --cookies-from-browser",
      "ERROR: Sign in to confirm you're not a bot",
      "ERROR: [youtube] xxxx: This content isn't available; No video formats found!",
    ] {
      let err = classify_ytdlp_failure(stderr, None);
      assert_eq!(
        err.kind(),
        &YtDlpErrorKind::ExtractionBlocked,
        "should be ExtractionBlocked, not auth: {stderr}"
      );
      assert!(!err.is_auth_error(), "must not open login window: {stderr}");
    }
    }

    #[test]
    fn classify_true_auth_still_auth() {
        for stderr in [
            "ERROR: This video requires login",
            "ERROR: unable to download: HTTP Error 401",
            "ERROR: Private video. Sign in if you've been granted access to view this video",
            "ERROR: HTTP Error 403: Forbidden",
            "ERROR: Join this channel to get access to members-only content",
        ] {
            let err = classify_ytdlp_failure(stderr, None);
            assert_eq!(
                err.kind(),
                &YtDlpErrorKind::AuthRequired,
                "should stay auth: {stderr}"
            );
            assert!(err.is_auth_error());
        }
    }

    // ---- reviewer fix (Slice H): 真auth marker 拡張 + 分類順序 (複合ケース) ----

    #[test]
    fn classify_extended_auth_markers_are_auth() {
        // 指摘2: 追加した真auth marker の exact strings が AuthRequired になること。
        for stderr in [
            "ERROR: Sign in to view this video",
            "ERROR: This video is private",
            "ERROR: Join this channel to get access to members-only content",
        ] {
            let err = classify_ytdlp_failure(stderr, None);
            assert_eq!(
                err.kind(),
                &YtDlpErrorKind::AuthRequired,
                "should be auth: {stderr}"
            );
            assert!(err.is_auth_error(), "should open login window: {stderr}");
        }
    }

    #[test]
    fn classify_composite_auth_plus_no_formats_prefers_auth() {
        // 指摘1: 複合 stderr (真auth + no formats) は auth が勝つ。
        // ExtractionBlocked に落ちると login window / auth helper が開かず private 動画が
        // 変換不能のまま放置されるため。
        for stderr in [
            "ERROR: HTTP Error 403: Forbidden ... No video formats found",
            "ERROR: Sign in to confirm your age ... No video formats found",
        ] {
            let err = classify_ytdlp_failure(stderr, None);
            assert_eq!(
                err.kind(),
                &YtDlpErrorKind::AuthRequired,
                "composite auth+no-formats must prefer auth: {stderr}"
            );
            assert!(err.is_auth_error(), "should open login window: {stderr}");
        }
    }

    #[test]
    fn classify_bot_and_no_formats_stay_extraction_blocked() {
        // bot 確認 (非auth) と auth 語なしの no formats は ExtractionBlocked 維持。
        for stderr in [
            "ERROR: Sign in to confirm you're not a bot",
            "ERROR: [youtube] xxxx: This content isn't available; No video formats found",
        ] {
            let err = classify_ytdlp_failure(stderr, None);
            assert_eq!(
                err.kind(),
                &YtDlpErrorKind::ExtractionBlocked,
                "should stay ExtractionBlocked: {stderr}"
            );
            assert!(!err.is_auth_error(), "must not open login window: {stderr}");
        }
    }
}
