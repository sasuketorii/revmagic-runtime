//! copy fast-path テスト (Slice E)。
//!
//! 入力が **既に最終形の MP3 (mp3 / 44100Hz / 2ch / 除去対象タグ無し)** で
//! `optimize_for_size == false` かつ split 指定なしのとき、[`convert_media_blocking`]
//! は再エンコードせず ffmpeg `-c copy` (stream copy) で出力するはず (fast-path)。
//! 1 つでも条件を欠く入力は従来 `libmp3lame` 再エンコード経路に倒れるはず。
//!
//! ## 検証方法 (実 ffmpeg/ffprobe・ネット非依存)
//! 公開 API を実 ffmpeg/ffprobe で回し、出力 MP3 の性質を ffprobe / バイト比較で観測する。
//!
//! - **copy が使われた証拠 (positive / optimize 以外)**: copy 引数は
//!   `-map 0:a:0 -vn -c copy -map_metadata -1` で stream copy する。`-map_metadata -1` を
//!   付けたため入力 metadata (`encoder` 等) は出力で落ちる/置換され **バイト同一にはならない**。
//!   そこで copy 経路は (1) **引数レベル検証** (`build_single_copy_args` が `-c copy` を含み
//!   `libmp3lame` を含まず `-map 0:a:0` と `-map_metadata -1` を持つ) と (2) **出力性質検証**
//!   (codec=mp3 / 44100 / 2ch かつ user metadata 除去) で決定的に確認する。
//! - **再エンコードが使われた証拠 (negative a/b/c/d)**:
//!   - (a) タグ有り: `-map_metadata -1` で消えるはずの user タグが出力から消えている
//!     (copy なら残る)。
//!   - (b) 48000Hz: 出力が 44100Hz に正規化されている (copy なら 48000 のまま)。
//!   - (c) mono: 出力が 2ch に正規化されている (copy なら mono のまま)。
//!   - (d) optimize=true: 入力は copy 適格な形状だが optimize 指定で再エンコードされる
//!     → 出力が入力とバイト非同一 (copy なら同一)。
//!   - (e) split 指定: copy fast-path 対象外。split 経路で複数セグメントが出力される
//!     (= run_single_conversion の copy 分岐に入らない)。
//!
//! ## ffmpeg 依存方針 (CRITICAL, Slice C と同様)
//! ffmpeg/ffprobe 不在時は skip / `#[ignore]` せず **fail-closed** (panic でテスト fail)。
//! PATH 上の実行可能バイナリを BinaryResolver で解決し、実行不能なら fail-closed にする。

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use revmagic_core::{
    build_single_copy_args, convert_media_blocking, BinaryResolver, ConversionRequest,
    FallbackBinaryResolver, Progress,
};

// ---------------------------------------------------------------------------
// 同梱バイナリ解決 (Slice C / golden と同方針, fail-closed)
// ---------------------------------------------------------------------------

fn bundled_resolver() -> FallbackBinaryResolver {
    FallbackBinaryResolver::new()
}

/// fail-closed 解決 + 同梱バイナリであることの assert。
fn resolve_or_fail(resolver: &FallbackBinaryResolver, binary: &str) -> PathBuf {
    let resolved = resolver
        .resolve(binary)
        .unwrap_or_else(|error| panic!("fail-closed: {binary} を解決できませんでした: {error}"));
    assert_bundled_usable(&resolved, binary);
    resolved
}

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
        "同梱 {binary} が 0 byte です: {}",
        path.display()
    );
    assert!(
        metadata.permissions().mode() & 0o111 != 0,
        "同梱 {binary} に実行可能 bit がありません: {}",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// tmp / fixture ヘルパ
// ---------------------------------------------------------------------------

struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "revmagic-copy-fastpath-{tag}-{}-{}",
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

fn noop_progress(_: &Progress) -> Result<(), String> {
    Ok(())
}

/// 任意の sample_rate / channels / 追加引数 (メタタグ等) を持つ MP3 fixture を生成する。
fn make_mp3(
    ffmpeg: &Path,
    out: &Path,
    sample_rate: u32,
    channels: u32,
    extra_args: &[&str],
    duration_seconds: u32,
) {
    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("sine=frequency=440:sample_rate={sample_rate}:duration={duration_seconds}"),
        "-ac",
        &channels.to_string(),
    ]);
    cmd.args(extra_args);
    cmd.args(["-c:a", "libmp3lame", "-b:a", "128k"]).arg(out);
    let status = cmd
        .status()
        .unwrap_or_else(|error| panic!("fixture 生成 ffmpeg の実行に失敗: {error}"));
    assert!(
        status.success(),
        "fixture 生成 ffmpeg が異常終了 (out={})",
        out.display()
    );
    assert!(
        out.exists(),
        "fixture が生成されていません: {}",
        out.display()
    );
}

/// multi-audio fixture を生成する: a:0 = mp3 / 44100Hz / 2ch / 除去対象タグ無し、
/// a:1 = AAC / 別形状。`-c copy` で a:1 を拾うと MP3 muxer が AAC を mux できず失敗するため、
/// reviewer 指摘 (multi-audio で a:0 を固定 copy できること) の回帰テストに使う。
/// 単一 stream MP3 コンテナには 2 stream を入れられないので Matroska (.mkv) に格納する。
fn make_multi_audio_mkv(ffmpeg: &Path, out: &Path, duration_seconds: u32) {
    let status = Command::new(ffmpeg)
        .args([
            "-y",
            // 入力0: a:0 になる音声 (mp3/44100/2ch)。
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=44100:duration={duration_seconds}"),
            // 入力1: a:1 になる別音声 (AAC/48000/mono 相当)。
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=880:sample_rate=48000:duration={duration_seconds}"),
            "-map",
            "0:a",
            "-map",
            "1:a",
            // a:0 = mp3 / 2ch、a:1 = aac / mono。
            "-c:a:0",
            "libmp3lame",
            "-ac:a:0",
            "2",
            "-c:a:1",
            "aac",
            "-ac:a:1",
            "1",
            // 入力由来 metadata を落として a:0 を copy 適格形状にする。
            "-map_metadata",
            "-1",
        ])
        .arg(out)
        .status()
        .unwrap_or_else(|error| panic!("multi-audio fixture 生成 ffmpeg の実行に失敗: {error}"));
    assert!(
        status.success(),
        "multi-audio fixture 生成 ffmpeg が異常終了 (out={})",
        out.display()
    );
    assert!(
        out.exists(),
        "multi-audio fixture が生成されていません: {}",
        out.display()
    );
}

/// ffprobe で先頭オーディオストリームの codec/sample_rate/channels と全タグを取得。
struct Probe {
    sample_rate: u32,
    channels: u32,
    codec_name: String,
    /// format/stream の全タグ dump (小文字化)。
    raw_tags_dump: String,
}

fn probe(ffprobe: &Path, file: &Path) -> Probe {
    let stream_out = run_ffprobe(
        ffprobe,
        &[
            OsStr::new("-v"),
            OsStr::new("error"),
            OsStr::new("-select_streams"),
            OsStr::new("a:0"),
            OsStr::new("-show_entries"),
            OsStr::new("stream=sample_rate,channels,codec_name"),
            OsStr::new("-of"),
            OsStr::new("default=noprint_wrappers=1"),
        ],
        file,
    );
    let mut sample_rate = 0u32;
    let mut channels = 0u32;
    let mut codec_name = String::new();
    for line in stream_out.lines() {
        if let Some(v) = line.strip_prefix("sample_rate=") {
            sample_rate = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("channels=") {
            channels = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("codec_name=") {
            codec_name = v.trim().to_string();
        }
    }
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
    Probe {
        sample_rate,
        channels,
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
        "ffprobe が異常終了 (file={}): {}",
        file.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn list_mp3s(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(folder)
        .unwrap_or_else(|error| panic!("出力フォルダを読めません {}: {error}", folder.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mp3"))
        .collect();
    files.sort();
    files
}

fn bytes_identical(a: &Path, b: &Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// 変換を実行し、出力フォルダ内の単一 MP3 パスを返す (single 経路前提)。
fn convert_single(
    resolver: &FallbackBinaryResolver,
    input: &Path,
    output_dir: &Path,
    optimize_for_size: bool,
) -> PathBuf {
    let summary = convert_media_blocking(
        resolver,
        ConversionRequest {
            input_path: input.to_string_lossy().into_owned(),
            output_directory: output_dir.to_string_lossy().into_owned(),
            split_minutes: None,
            optimize_for_size,
        },
        &noop_progress,
    )
    .expect("変換に失敗しました");
    assert_eq!(summary.file_count, 1, "single 変換は 1 ファイル出力のはず");
    let mp3s = list_mp3s(Path::new(&summary.output_folder));
    assert_eq!(
        mp3s.len(),
        1,
        "single 変換の出力 mp3 が 1 件ではありません: {mp3s:?}"
    );
    mp3s.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// positive: copy fast-path が発火する
// ---------------------------------------------------------------------------

/// 引数列に `needle` (連続部分列) が含まれるか。`-map 0:a:0` のような順序付き
/// 2 トークン検証に使う。
fn args_contain_subsequence(args: &[std::ffi::OsString], needle: &[&str]) -> bool {
    if needle.is_empty() {
        return true;
    }
    args.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle.iter())
            .all(|(have, want)| have.to_string_lossy() == *want)
    })
}

fn args_contain_token(args: &[std::ffi::OsString], token: &str) -> bool {
    args.iter().any(|a| a.to_string_lossy() == token)
}

/// copy 引数の不変条件 (Slice E reviewer 指摘): `-c copy` かつ libmp3lame 非使用、
/// probe 対象と一致する `-map 0:a:0`、metadata 等価のための `-map_metadata -1`。
fn assert_copy_args_invariants(args: &[std::ffi::OsString]) {
    assert!(
        args_contain_subsequence(args, &["-c", "copy"]),
        "copy 引数に `-c copy` がありません: {args:?}"
    );
    assert!(
        !args_contain_token(args, "libmp3lame"),
        "copy 引数に libmp3lame が含まれています (再エンコードになっている): {args:?}"
    );
    assert!(
        args_contain_subsequence(args, &["-map", "0:a:0"]),
        "copy 引数に `-map 0:a:0` がありません (probe 対象 a:0 と copy 対象が不一致): {args:?}"
    );
    assert!(
        args_contain_subsequence(args, &["-map_metadata", "-1"]),
        "copy 引数に `-map_metadata -1` がありません (metadata 非等価): {args:?}"
    );
}

/// mp3 / 44100Hz / 2ch / 除去対象タグ無し / optimize無 / split無 → `-c copy`。
///
/// `-map_metadata -1` を copy にも付けたため出力は入力とバイト同一にならない。よって
/// (1) 引数レベル検証 (`-c copy` / libmp3lame 非使用 / `-map 0:a:0` / `-map_metadata -1`) と
/// (2) 出力性質検証 (mp3/44100/2ch かつ user metadata 除去) で copy 経路を決定的に確認する。
#[test]
fn positive_clean_mp3_uses_copy() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("positive");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    // (1) 引数レベル検証: copy 引数の不変条件を deterministic に確認 (ffmpeg 非依存)。
    assert_copy_args_invariants(&build_single_copy_args(
        Path::new("/in/clean.mp3"),
        Path::new("/out/clean.mp3"),
    ));

    // 既に最終形 (44100/2ch/user タグ無し)。`-map_metadata -1` で除去対象タグを除いておく。
    let input = input_dir.join("clean.mp3");
    make_mp3(&ffmpeg, &input, 44100, 2, &["-map_metadata", "-1"], 3);

    // fixture 前提 assert (vacuous pass 防止): 入力が mp3/44100/2ch かつ user タグ無し。
    let in_probe = probe(&ffprobe, &input);
    assert_eq!(
        in_probe.codec_name, "mp3",
        "fixture 前提違反: 入力 codec が mp3 でない"
    );
    assert_eq!(
        in_probe.sample_rate, 44100,
        "fixture 前提違反: 入力 sample_rate≠44100"
    );
    assert_eq!(in_probe.channels, 2, "fixture 前提違反: 入力 channels≠2");
    for needle in ["title=", "artist=", "comment="] {
        assert!(
            !in_probe.raw_tags_dump.contains(needle),
            "fixture 前提違反: 入力に除去対象タグ {needle} が残っています (dump={})",
            in_probe.raw_tags_dump
        );
    }

    let output = convert_single(&resolver, &input, &output_dir, false);

    // (2) 出力性質検証: copy で stream はそのまま、形状 (mp3/44100/2ch) を維持。
    let out_probe = probe(&ffprobe, &output);
    assert_eq!(
        out_probe.codec_name, "mp3",
        "copy 出力 codec が mp3 でない: {}",
        out_probe.codec_name
    );
    assert_eq!(
        out_probe.sample_rate, 44100,
        "copy 出力 sample_rate≠44100: {}",
        out_probe.sample_rate
    );
    assert_eq!(
        out_probe.channels, 2,
        "copy 出力 channels≠2: {}",
        out_probe.channels
    );
    // metadata 等価性: `-map_metadata -1` で user タグは出力に残らない。
    for needle in ["title=revmagic", "artist=revmagic", "comment=revmagic"] {
        assert!(
            !out_probe.raw_tags_dump.contains(needle),
            "copy 出力に user metadata が残留しています (dump={})",
            out_probe.raw_tags_dump
        );
    }
}

// ---------------------------------------------------------------------------
// negative (a): タグ有り → 再エンコード経路
// ---------------------------------------------------------------------------

#[test]
fn negative_tagged_input_reencodes() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("tagged");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    // 44100/2ch だが除去対象 user タグ有り。
    let input = input_dir.join("tagged.mp3");
    make_mp3(
        &ffmpeg,
        &input,
        44100,
        2,
        &[
            "-metadata",
            "title=REVMAGIC_E_TITLE",
            "-metadata",
            "artist=REVMAGIC_E_ARTIST",
        ],
        3,
    );

    // fixture 前提: 入力に title タグが実在する。
    let in_probe = probe(&ffprobe, &input);
    assert!(
        in_probe.raw_tags_dump.contains("revmagic_e_title"),
        "fixture 前提違反: 入力に title タグが焼かれていません (dump={})",
        in_probe.raw_tags_dump
    );

    let output = convert_single(&resolver, &input, &output_dir, false);

    // 再エンコード経路 (`-map_metadata -1`) の証拠: user タグが出力から消えている。
    // copy ならタグが残るので、これが消えていれば libmp3lame 再エンコード経路。
    let out_probe = probe(&ffprobe, &output);
    assert!(
    !out_probe.raw_tags_dump.contains("revmagic_e_title")
      && !out_probe.raw_tags_dump.contains("revmagic_e_artist"),
    "タグ有り入力が copy されています (タグが残留 = 従来再エンコード経路に倒れていない): dump={}",
    out_probe.raw_tags_dump
  );
    // copy でないこと (バイト非同一) も併せて確認。
    assert!(
        !bytes_identical(&input, &output),
        "タグ有り入力が copy されています (出力が入力とバイト同一)"
    );
}

// ---------------------------------------------------------------------------
// negative (b): sample_rate≠44100 (48000) → 再エンコードで 44100 正規化
// ---------------------------------------------------------------------------

#[test]
fn negative_non_44100_reencodes() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("sr48000");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    let input = input_dir.join("sr48000.mp3");
    make_mp3(&ffmpeg, &input, 48000, 2, &["-map_metadata", "-1"], 3);

    let in_probe = probe(&ffprobe, &input);
    assert_eq!(
        in_probe.sample_rate, 48000,
        "fixture 前提違反: 入力 sample_rate≠48000"
    );

    let output = convert_single(&resolver, &input, &output_dir, false);

    // 再エンコードで 44100 に正規化されている (copy なら 48000 のまま)。
    let out_probe = probe(&ffprobe, &output);
    assert_eq!(
        out_probe.sample_rate, 44100,
        "48000Hz 入力が copy されています (出力 sample_rate が 44100 に正規化されていない: {})",
        out_probe.sample_rate
    );
}

// ---------------------------------------------------------------------------
// negative (c): channels≠2 (mono) → 再エンコードで 2ch 正規化
// ---------------------------------------------------------------------------

#[test]
fn negative_mono_reencodes() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("mono");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    let input = input_dir.join("mono.mp3");
    make_mp3(&ffmpeg, &input, 44100, 1, &["-map_metadata", "-1"], 3);

    let in_probe = probe(&ffprobe, &input);
    assert_eq!(
        in_probe.channels, 1,
        "fixture 前提違反: 入力 channels≠1 (mono)"
    );

    let output = convert_single(&resolver, &input, &output_dir, false);

    // 再エンコードで 2ch に正規化されている (copy なら mono のまま)。
    let out_probe = probe(&ffprobe, &output);
    assert_eq!(
        out_probe.channels, 2,
        "mono 入力が copy されています (出力 channels が 2 に正規化されていない: {})",
        out_probe.channels
    );
}

// ---------------------------------------------------------------------------
// negative (d): optimize=true → copy 適格形状でも再エンコード
// ---------------------------------------------------------------------------

#[test]
fn negative_optimize_reencodes() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("optimize");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    // 形状は copy 適格 (44100/2ch/タグ無し) だが optimize=true で再エンコードのはず。
    let input = input_dir.join("opt.mp3");
    make_mp3(&ffmpeg, &input, 44100, 2, &["-map_metadata", "-1"], 3);

    let in_probe = probe(&ffprobe, &input);
    assert_eq!(in_probe.sample_rate, 44100);
    assert_eq!(in_probe.channels, 2);

    let output = convert_single(&resolver, &input, &output_dir, true);

    // optimize は再エンコード (ビットレート最適化) が必要 → copy されず出力は入力とバイト非同一。
    assert!(
        !bytes_identical(&input, &output),
        "optimize=true でも copy されています (出力が入力とバイト同一)"
    );
    // optimize は文字起こし向け固定プロファイルへ確実に縮小する。
    let out_probe = probe(&ffprobe, &output);
    assert_eq!(out_probe.sample_rate, 24000);
    assert_eq!(out_probe.channels, 1);
}

// ---------------------------------------------------------------------------
// negative (e): split 指定 → copy fast-path 対象外 (split 経路)
// ---------------------------------------------------------------------------

#[test]
fn negative_split_does_not_use_single_copy() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("split");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    // copy 適格形状の長め入力。split 指定で single の copy 分岐には入らないはず。
    let input = input_dir.join("split_src.mp3");
    make_mp3(&ffmpeg, &input, 44100, 2, &["-map_metadata", "-1"], 90);

    let in_probe = probe(&ffprobe, &input);
    assert_eq!(in_probe.sample_rate, 44100);
    assert_eq!(in_probe.channels, 2);

    let summary = convert_media_blocking(
        &resolver,
        ConversionRequest {
            input_path: input.to_string_lossy().into_owned(),
            output_directory: output_dir.to_string_lossy().into_owned(),
            split_minutes: Some(1), // 60s 境界 → 90s 入力は 2 セグメント
            optimize_for_size: false,
        },
        &noop_progress,
    )
    .expect("split 変換に失敗しました");

    // split 経路 (run_split_conversion) を通り、複数セグメントが出力される。
    // single の copy 分岐に入っていたら 1 ファイル (segment-*.mp3 命名でもない) になる。
    let mp3s = list_mp3s(Path::new(&summary.output_folder));
    assert!(
        mp3s.len() >= 2,
        "split 指定が single 経路に流れています (segment 数={}): {mp3s:?}",
        mp3s.len()
    );
    assert_eq!(summary.file_count, mp3s.len());
    // segment-*.mp3 命名であること (split 経路の出力テンプレート)。
    for f in &mp3s {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name.starts_with("segment-"),
            "split 出力が segment-*.mp3 命名ではありません (= split 経路でない): {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// multi-audio: a:0=mp3/44100/2ch (タグ無) + a:1=AAC/別形状
//   → probe 対象 a:0 が copy 適格。`-map 0:a:0` で a:0 を固定 copy して成功する。
//     (`-map 0:a:0` が無いと ffmpeg 既定が a:1=AAC を選び MP3 muxer で失敗しうる回帰)
// ---------------------------------------------------------------------------

fn probe_stream_codec(ffprobe: &Path, file: &Path, stream: &str) -> String {
    let out = run_ffprobe(
        ffprobe,
        &[
            OsStr::new("-v"),
            OsStr::new("error"),
            OsStr::new("-select_streams"),
            OsStr::new(stream),
            OsStr::new("-show_entries"),
            OsStr::new("stream=codec_name"),
            OsStr::new("-of"),
            OsStr::new("default=noprint_wrappers=1:nokey=1"),
        ],
        file,
    );
    out.trim().to_string()
}

#[test]
fn multi_audio_copies_first_stream_and_succeeds() {
    let resolver = bundled_resolver();
    let ffmpeg = resolve_or_fail(&resolver, "ffmpeg");
    let ffprobe = resolve_or_fail(&resolver, "ffprobe");

    let tmp = TmpDir::new("multiaudio");
    let input_dir = tmp.path.join("in");
    let output_dir = tmp.path.join("out");
    fs::create_dir_all(&input_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    // a:0=mp3/44100/2ch/タグ無 + a:1=AAC/48000/mono の multi-audio 入力。
    let input = input_dir.join("multi.mkv");
    make_multi_audio_mkv(&ffmpeg, &input, 3);

    // fixture 前提 (vacuous pass 防止): a:0 が mp3/44100/2ch、a:1 が aac (非mp3)。
    let a0 = probe(&ffprobe, &input);
    assert_eq!(
        a0.codec_name, "mp3",
        "fixture 前提違反: a:0 codec が mp3 でない"
    );
    assert_eq!(
        a0.sample_rate, 44100,
        "fixture 前提違反: a:0 sample_rate≠44100"
    );
    assert_eq!(a0.channels, 2, "fixture 前提違反: a:0 channels≠2");
    let a1_codec = probe_stream_codec(&ffprobe, &input, "a:1");
    assert_eq!(
        a1_codec, "aac",
        "fixture 前提違反: a:1 codec が aac でない (multi-audio になっていない): {a1_codec}"
    );

    // probe 対象 a:0 が copy 適格 → `-map 0:a:0` で a:0 を固定 copy。a:1(AAC) を拾わず成功するはず。
    let output = convert_single(&resolver, &input, &output_dir, false);

    // 出力は a:0 由来の mp3/44100/2ch 単一 stream。
    let out_probe = probe(&ffprobe, &output);
    assert_eq!(
        out_probe.codec_name, "mp3",
        "multi-audio copy 出力 codec が mp3 でない (a:1=AAC を拾った疑い): {}",
        out_probe.codec_name
    );
    assert_eq!(
        out_probe.sample_rate, 44100,
        "multi-audio copy 出力 sample_rate≠44100 (a:1=48000 を拾った疑い): {}",
        out_probe.sample_rate
    );
    assert_eq!(
        out_probe.channels, 2,
        "multi-audio copy 出力 channels≠2 (a:1=mono を拾った疑い): {}",
        out_probe.channels
    );
    // 出力 stream はちょうど 1 本 (a:1 を取り込んでいない)。
    let n_streams = probe_stream_codec(&ffprobe, &output, "a")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(
        n_streams, 1,
        "multi-audio copy 出力に音声 stream が 2 本以上 (a:1 を取り込んだ): {n_streams}"
    );
}
