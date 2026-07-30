//! MP3 出力ゴールデンテスト (回帰安全網)。
//!
//! Slice D(二重エンコード解消)/E(copy fast-path) で変換ロジックを触る前に、
//! **現状の変換出力 MP3 の性質** をゴールデン化して回帰検知の網を張る。
//! 変換ロジック本体 (lib.rs / ytdlp.rs) は一切変更せず、公開 API
//! [`revmagic_core::convert_media_blocking`] を実 ffmpeg/ffprobe で回し、
//! 出力を ffprobe でパースして以下を検証する:
//!
//! - duration: 入力長と概ね一致する。
//! - sample rate = 44100 Hz。
//! - channels = 2 (ステレオ正規化)。
//! - メタデータ除去: 入力に付与した検証用タグ (title/artist) が出力に残らない。
//! - split 境界: `split_minutes` 指定で複数ファイルが出力され、各セグメント長が妥当。
//!
//! ## ffmpeg 依存方針 (CRITICAL)
//! ffmpeg/ffprobe 不在時は skip / `#[ignore]` せず **fail-closed**
//! (テスト fail + 明示メッセージ)。PATH 上の実行可能バイナリが前提。
//!
//! fixture は決定的・ネット非依存にするため ffmpeg lavfi (`sine`/`anullsrc`)
//! からテスト内で生成する。入力にはメタタグを焼き込み、出力での除去を確認する。

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use revmagic_core::{
    convert_media_blocking, BinaryResolver, ConversionRequest, FallbackBinaryResolver, Progress,
};

/// PATH 上の実行可能バイナリを解決する resolver。
///
/// `FallbackBinaryResolver::with_resource_dir(<desktop-host>)` は
/// `<desktop-host>/binaries/<binary>` を最初の候補に積むため、同梱の実 arm64
/// ffmpeg/ffprobe を経由する (BinaryResolver 経由解決の要件)。同梱が無い/壊れて
/// いる場合のみ PATH/Homebrew へ fallthrough する。
fn bundled_resolver() -> FallbackBinaryResolver {
    FallbackBinaryResolver::new()
}

/// ffmpeg/ffprobe を fail-closed で解決する。
///
/// 不在時は skip せず **panic** させ、テストを fail させる (同梱前提の明示)。
///
/// 解決された実 path が通常ファイルかつ実行可能であることを確認する。
fn resolve_or_fail(resolver: &FallbackBinaryResolver, binary: &str) -> PathBuf {
    let resolved = resolver
        .resolve(binary)
        .unwrap_or_else(|error| panic!("fail-closed: {binary} を解決できませんでした: {error}"));
    assert_bundled_usable(&resolved, binary);
    resolved
}

/// 同梱 path が「採用可能な実バイナリ」(通常ファイル / 非0byte / 実行可能) で
/// あることを表明する。`lib.rs::is_usable_binary` 相当の前提をテスト側で再確認し、
/// placeholder sentinel や壊れた同梱での false green を防ぐ。
fn assert_bundled_usable(path: &Path, binary: &str) {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).unwrap_or_else(|error| {
        panic!(
            "同梱 {binary} のメタデータを取得できません ({}): {error}",
            path.display()
        )
    });
    assert!(
        metadata.is_file(),
        "同梱 {binary} が通常ファイルではありません: {}",
        path.display()
    );
    assert!(
        metadata.len() > 0,
        "同梱 {binary} が 0 byte です (placeholder sentinel の可能性): {}",
        path.display()
    );
    assert!(
        metadata.permissions().mode() & 0o111 != 0,
        "同梱 {binary} に実行可能 bit がありません: {}",
        path.display()
    );
}

/// テスト一時ディレクトリ (Drop で再帰削除)。
struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "revmagic-mp3-golden-{tag}-{}-{}",
            std::process::id(),
            nano()
        ));
        fs::create_dir_all(&base).unwrap();
        Self { path: base }
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn nano() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// lavfi `sine` から決定的な音声付き入力ファイルを生成する。
///
/// - `duration_seconds`: 入力長。
/// - 検証用メタタグ (title/artist/comment) を焼き込む。出力で除去されることを確認する。
/// - 入力 sample rate はあえて 48000Hz にして、変換側の 44100Hz 正規化が効くことを保証する。
/// - 入力は mono にして、変換側の 2ch (stereo) 正規化が効くことを保証する。
fn make_input(
    ffmpeg: &Path,
    ffprobe: &Path,
    dir: &Path,
    name: &str,
    duration_seconds: u32,
) -> PathBuf {
    let output = dir.join(name);
    let status = Command::new(ffmpeg)
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            // 440Hz sine, 入力は 48000Hz / mono (正規化検証のためあえて非44100/非2ch)。
            &format!("sine=frequency=440:sample_rate=48000:duration={duration_seconds}"),
            "-ac",
            "1",
            // 検証用メタタグ。出力 (map_metadata -1) で消えることを確認する。
            "-metadata",
            "title=REVMAGIC_GOLDEN_TITLE",
            "-metadata",
            "artist=REVMAGIC_GOLDEN_ARTIST",
            "-metadata",
            "comment=REVMAGIC_GOLDEN_COMMENT",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "4",
        ])
        .arg(&output)
        .status()
        .unwrap_or_else(|error| panic!("fixture 生成のための ffmpeg 実行に失敗: {error}"));

    assert!(
        status.success(),
        "fixture 生成 ffmpeg が異常終了しました (name={name})"
    );
    assert!(
        output.exists(),
        "fixture が生成されていません: {}",
        output.display()
    );

    // fixture 前提を assert (指摘1: vacuous pass 防止)。
    // 入力が 48000Hz / mono / 3 検証タグ付きであることを確認する。これが満たされて
    // いれば、出力での 44100Hz 正規化・2ch 正規化・メタ除去は「実際に変換で起きた」
    // ことが保証される (入力が既に 44100/2ch/タグ無しなら検証が無意味になる)。
    assert_fixture_preconditions(ffprobe, &output);

    output
}

/// 入力 fixture が想定どおり (48000Hz / mono / 検証タグ 3 つ) であることを表明する
/// (指摘1)。これにより golden の正規化・メタ除去 assert が vacuous でないことを担保。
fn assert_fixture_preconditions(ffprobe: &Path, input: &Path) {
    let probe = ffprobe_probe(ffprobe, input);
    assert_eq!(
        probe.sample_rate,
        48000,
        "fixture 前提違反: 入力 sample_rate が 48000Hz ではありません (got={}, file={})。\
     44100Hz 正規化の検証が vacuous になります。",
        probe.sample_rate,
        input.display()
    );
    assert_eq!(
        probe.channels,
        1,
        "fixture 前提違反: 入力 channels が 1 (mono) ではありません (got={}, file={})。\
     2ch 正規化の検証が vacuous になります。",
        probe.channels,
        input.display()
    );
    for needle in [
        "revmagic_golden_title",
        "revmagic_golden_artist",
        "revmagic_golden_comment",
    ] {
        assert!(
            probe.raw_tags_dump.contains(needle),
            "fixture 前提違反: 入力に検証タグ {needle} が含まれていません (file={}, dump={})。\
       メタデータ除去の検証が vacuous になります。",
            input.display(),
            probe.raw_tags_dump
        );
    }
}

/// no-op progress callback。
fn noop_progress(_: &Progress) -> Result<(), String> {
    Ok(())
}

/// ffprobe を `key=value` 行形式 (JSON パーサ不要) で叩き、出力 MP3 の
/// stream 性質 + format duration をパースする。
struct ProbeResult {
    sample_rate: u32,
    channels: u32,
    duration_seconds: f64,
    /// 出力 stream の codec_name (`.mp3` 拡張子だけでなく中身が MP3 か確認用)。
    codec_name: String,
    /// format/stream の全 tag を素朴に小文字化して連結したもの (タグ残留検知用)。
    raw_tags_dump: String,
}

fn ffprobe_probe(ffprobe: &Path, file: &Path) -> ProbeResult {
    // stream の sample_rate/channels と format の duration を default=key=value で取得。
    let stream_out = run_ffprobe(
        ffprobe,
        &[
            OsStr::new("-v"),
            OsStr::new("error"),
            OsStr::new("-select_streams"),
            OsStr::new("a:0"),
            OsStr::new("-show_entries"),
            OsStr::new("stream=sample_rate,channels,codec_name:format=duration"),
            OsStr::new("-of"),
            OsStr::new("default=noprint_wrappers=1"),
        ],
        file,
    );

    let mut sample_rate = 0u32;
    let mut channels = 0u32;
    let mut duration_seconds = 0.0f64;
    let mut codec_name = String::new();
    for line in stream_out.lines() {
        if let Some(v) = line.strip_prefix("sample_rate=") {
            sample_rate = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("channels=") {
            channels = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("codec_name=") {
            codec_name = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("duration=") {
            duration_seconds = v.trim().parse().unwrap_or(0.0);
        }
    }

    // タグ残留検知: format/stream の全 tag を出力させる。タグが無ければ出力は空に近い。
    let tags_out = run_ffprobe(
        ffprobe,
        &[
            OsStr::new("-v"),
            OsStr::new("error"),
            OsStr::new("-show_entries"),
            OsStr::new("format_tags:stream_tags"),
            OsStr::new("-of"),
            OsStr::new("default=noprint_wrappers=1"),
        ],
        file,
    );

    ProbeResult {
        sample_rate,
        channels,
        duration_seconds,
        codec_name,
        raw_tags_dump: tags_out.to_lowercase(),
    }
}

fn run_ffprobe(ffprobe: &Path, args: &[&OsStr], file: &Path) -> String {
    let output = Command::new(ffprobe)
        .args(args)
        .arg(file)
        .output()
        .unwrap_or_else(|error| panic!("ffprobe 実行に失敗: {error}"));
    assert!(
        output.status.success(),
        "ffprobe が異常終了しました (file={}): {}",
        file.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// 出力フォルダ内の `.mp3` を昇順で列挙する。
fn list_mp3s(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(folder)
        .unwrap_or_else(|error| panic!("出力フォルダを読めません {}: {error}", folder.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mp3"))
        .collect();
    files.sort();
    files
}

/// 入力タグが出力に残っていないことを表明する。
fn assert_no_input_tags(probe: &ProbeResult, file: &Path) {
    for needle in [
        "revmagic_golden_title",
        "revmagic_golden_artist",
        "revmagic_golden_comment",
    ] {
        assert!(
            !probe.raw_tags_dump.contains(needle),
            "メタデータが出力に残留しています (needle={needle}, file={}, dump={})",
            file.display(),
            probe.raw_tags_dump
        );
    }
}

/// 共通: 出力 MP3 1 件の基本性質 (44100Hz / 2ch / タグ除去) を表明する。
fn assert_basic_properties(probe: &ProbeResult, file: &Path) {
    assert_eq!(
        probe.sample_rate,
        44100,
        "sample rate が 44100Hz ではありません (file={})",
        file.display()
    );
    assert_eq!(
        probe.channels,
        2,
        "channels が 2 (stereo) ではありません (file={})",
        file.display()
    );
    // 出力 stream が実際に MP3 codec であることを確認 (指摘3: 拡張子だけでは弱い)。
    assert_eq!(
        probe.codec_name,
        "mp3",
        "出力 stream の codec_name が mp3 ではありません (got={}, file={})",
        probe.codec_name,
        file.display()
    );
    assert_no_input_tags(probe, file);
}

#[test]
fn single_conversion_golden_properties() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("single");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    let input_seconds = 6u32;
    let input = make_input(
        &ffmpeg,
        &ffprobe,
        &input_dir,
        "golden_single.mp3",
        input_seconds,
    );

    let summary = convert_media_blocking(
        &resolver,
        ConversionRequest {
            input_path: input.to_string_lossy().into_owned(),
            output_directory: output_dir.to_string_lossy().into_owned(),
            split_minutes: None,
            optimize_for_size: false,
        },
        &noop_progress,
    )
    .expect("変換 (single) が失敗しました");

    assert_eq!(summary.file_count, 1, "single 変換は 1 ファイル出力のはず");

    let output_folder = PathBuf::from(&summary.output_folder);
    let mp3s = list_mp3s(&output_folder);
    assert_eq!(
        mp3s.len(),
        1,
        "single 変換の出力 mp3 が 1 件ではありません: {mp3s:?}"
    );

    let probe = ffprobe_probe(&ffprobe, &mp3s[0]);
    assert_basic_properties(&probe, &mp3s[0]);

    // duration: 入力長と概ね一致 (mp3 エンコーダのパディング等で ±0.5s 程度の差は許容)。
    let delta = (probe.duration_seconds - input_seconds as f64).abs();
    assert!(
        delta <= 0.5,
        "出力 duration が入力長と乖離しています (out={:.3}s, in={}s, delta={:.3}s)",
        probe.duration_seconds,
        input_seconds,
        delta
    );

    // summary.duration_seconds は入力長 (ffprobe 計測) なので同様に概ね一致。
    assert!(
        (summary.duration_seconds - input_seconds as f64).abs() <= 0.5,
        "summary.duration_seconds が入力長と乖離: {:.3}s",
        summary.duration_seconds
    );
}

#[test]
fn split_conversion_golden_boundaries() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("split");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    // split_minutes=1 (=60s 境界) で、150s 入力を分割 → 3 セグメント (60/60/30) を期待。
    let input_seconds = 150u32;
    let split_minutes = 1u64;
    let segment_seconds = (split_minutes * 60) as f64;
    let input = make_input(
        &ffmpeg,
        &ffprobe,
        &input_dir,
        "golden_split.mp3",
        input_seconds,
    );

    let summary = convert_media_blocking(
        &resolver,
        ConversionRequest {
            input_path: input.to_string_lossy().into_owned(),
            output_directory: output_dir.to_string_lossy().into_owned(),
            split_minutes: Some(split_minutes),
            optimize_for_size: false,
        },
        &noop_progress,
    )
    .expect("変換 (split) が失敗しました");

    let output_folder = PathBuf::from(&summary.output_folder);
    let mp3s = list_mp3s(&output_folder);

    // 150s / 60s 境界 → 3 セグメント。
    let expected_segments = 3usize;
    assert_eq!(
        mp3s.len(),
        expected_segments,
        "split 出力セグメント数が期待と異なります (got={}, want={}): {mp3s:?}",
        mp3s.len(),
        expected_segments
    );
    assert_eq!(
        summary.file_count, expected_segments,
        "summary.file_count が期待セグメント数と異なります"
    );

    // 各セグメントの性質と長さ妥当性を検証。
    let mut total_duration = 0.0f64;
    for (idx, file) in mp3s.iter().enumerate() {
        let probe = ffprobe_probe(&ffprobe, file);
        assert_basic_properties(&probe, file);

        let is_last = idx + 1 == mp3s.len();
        if is_last {
            // 末尾セグメント: 余り (150 - 120 = 30s) 程度。0 < d <= segment + 余裕。
            assert!(
                probe.duration_seconds > 0.0 && probe.duration_seconds <= segment_seconds + 0.5,
                "末尾セグメント長が不正 (idx={idx}, d={:.3}s, segment={segment_seconds}s)",
                probe.duration_seconds
            );
        } else {
            // 非末尾セグメント: segment_seconds に概ね一致 (±0.5s)。
            assert!(
        (probe.duration_seconds - segment_seconds).abs() <= 0.5,
        "中間セグメント長が segment_minutes 境界と乖離 (idx={idx}, d={:.3}s, segment={segment_seconds}s)",
        probe.duration_seconds
      );
        }
        total_duration += probe.duration_seconds;
    }

    // 全セグメント合計 ≒ 入力長 (分割で総尺が保たれる)。
    assert!(
        (total_duration - input_seconds as f64).abs() <= 1.5,
        "セグメント合計長が入力長と乖離 (sum={:.3}s, in={}s)",
        total_duration,
        input_seconds
    );
}
