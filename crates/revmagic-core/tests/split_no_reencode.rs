//! Slice D: yt-dlp → split 経路の二重エンコード解消を **引数レベル** で証明する。
//!
//! ## 何を証明するか
//! 旧実装では yt-dlp が `--extract-audio --audio-format mp3` で 1 回エンコード
//! (encode #1) した後、split 指定時に core が `libmp3lame -ar 44100 -ac 2
//! -map_metadata -1` で **2 回目** のエンコード (encode #2) をかけていた
//! (= 二重エンコード)。
//!
//! 本テストは実ネット / 実 ffmpeg / 実 yt-dlp に依存せず、引数組み立て関数
//! (`build_ytdlp_args` / `build_segment_copy_args`) を直接呼び、組み立てられる
//! argv を deterministic に検証して以下を保証する:
//!
//! 1. **正規化が単一パスで担保される**: yt-dlp の argv に
//!    `--postprocessor-args ExtractAudio:-ar 44100 -ac 2 -map_metadata -1` が含まれ、
//!    ExtractAudio の単一エンコードパスで 44100Hz/2ch 正規化・メタ除去まで行う。
//! 2. **split で再エンコードしない**: split のための ffmpeg argv が
//!    `-c copy` (stream copy) の segment 方式であり、`libmp3lame` / `-q:a` /
//!    `base_audio_args` 由来の再エンコード引数を **一切含まない**。
//!
//! この 2 点で「最終出力は 44100/2ch・メタ除去された mp3」かつ「split は追加の
//! 再エンコードをしない」= エンコード 1 回のみ、が引数レベルで担保される。

use std::ffi::OsString;
use std::path::PathBuf;

use revmagic_core::{build_segment_copy_args, build_ytdlp_args};

/// `OsString` の argv を `String` の Vec へ落とす (突き合わせ用)。
fn to_strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

/// `flag value` の隣接ペアが argv に存在するか。
fn window_has(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

/// yt-dlp の単一エンコードパスで 44100Hz/2ch 正規化・メタ除去まで行う argv に
/// なっていることを引数レベルで確認する (正規化が単一パスで担保される証明)。
#[test]
fn ytdlp_single_pass_normalizes_via_postprocessor_args() {
    let template = PathBuf::from("/out/%(title)s.%(ext)s");
    let args = build_ytdlp_args(
        "UA/1.0",
        "https://ref/",
        "2",
        None,
        None,
        &template,
        "https://www.loom.com/share/abc",
    );
    let as_str = to_strings(&args);

    // ExtractAudio の単一 ffmpeg パスへ正規化引数を渡している。
    assert!(
        window_has(
            &as_str,
            "--postprocessor-args",
            "ExtractAudio:-ar 44100 -ac 2 -map_metadata -1"
        ),
        "yt-dlp argv に ExtractAudio 正規化用 --postprocessor-args が含まれていません: {as_str:?}"
    );

    // 正規化引数の中身を意味的に確認 (44100Hz / 2ch / メタ除去)。
    let pp = as_str
        .iter()
        .position(|a| a == "--postprocessor-args")
        .and_then(|i| as_str.get(i + 1))
        .expect("--postprocessor-args の値が見つかりません");
    assert!(
        pp.starts_with("ExtractAudio:"),
        "ExtractAudio にスコープされていません: {pp}"
    );
    assert!(
        pp.contains("-ar 44100"),
        "44100Hz 正規化が含まれていません: {pp}"
    );
    assert!(pp.contains("-ac 2"), "2ch 正規化が含まれていません: {pp}");
    assert!(
        pp.contains("-map_metadata -1"),
        "メタ除去が含まれていません: {pp}"
    );

    // 単一エンコードである前提: ExtractAudio は 1 回だけ (mp3 出力)。
    assert!(as_str.iter().any(|a| a == "--extract-audio"));
    assert!(window_has(&as_str, "--audio-format", "mp3"));
}

/// split のための ffmpeg argv が `-c copy` の segment 方式であり、libmp3lame での
/// 再エンコード引数を一切含まないことを引数レベルで確認する
/// (split で再エンコードしない = 二重エンコードしない証明)。
#[test]
fn split_uses_stream_copy_not_reencode() {
    let input = PathBuf::from("/tmp/in/normalized.mp3");
    let template = PathBuf::from("/tmp/out/segment-%03d.mp3");
    let args = build_segment_copy_args(&input, 10, &template);
    let as_str = to_strings(&args);

    // 二重エンコード解消の要: stream copy で分割している。
    assert!(
        window_has(&as_str, "-c", "copy"),
        "split argv に `-c copy` (stream copy) が含まれていません: {as_str:?}"
    );

    // segment 分割方式であり、境界秒数が split_minutes(=10) * 60 = 600 秒。
    assert!(
        window_has(&as_str, "-f", "segment"),
        "segment muxer が指定されていません: {as_str:?}"
    );
    assert!(
        window_has(&as_str, "-segment_time", "600"),
        "segment_time が split_minutes*60 になっていません: {as_str:?}"
    );
    assert!(
        window_has(&as_str, "-reset_timestamps", "1"),
        "reset_timestamps が指定されていません: {as_str:?}"
    );

    // **再エンコード引数を一切含まない** ことを明示的に確認。
    // libmp3lame / -q:a / -c:a / 正規化 (-ar 44100 / -ac 2) のいずれも無い。
    assert!(
        !as_str.iter().any(|a| a == "libmp3lame"),
        "split が libmp3lame で再エンコードしています (二重エンコード): {as_str:?}"
    );
    assert!(
        !as_str.iter().any(|a| a == "-c:a"),
        "split に音声コーデック指定 (-c:a) が混入しています (再エンコードの疑い): {as_str:?}"
    );
    assert!(
        !as_str.iter().any(|a| a == "-q:a"),
        "split に品質指定 (-q:a) が混入しています (再エンコードの疑い): {as_str:?}"
    );
    // 正規化引数は yt-dlp 単一パス側の責務であり、split では行わない。
    assert!(
        !as_str.iter().any(|a| a == "-ar"),
        "split に sample rate 正規化 (-ar) が混入しています (再エンコードの疑い): {as_str:?}"
    );
    assert!(
        !as_str.iter().any(|a| a == "-ac"),
        "split に channels 正規化 (-ac) が混入しています (再エンコードの疑い): {as_str:?}"
    );
}

/// 入力 (yt-dlp 出力) が argv の `-i` 入力に渡り、出力テンプレートが segment
/// 連番になっていることを確認する (経路の取り違え防止)。
#[test]
fn split_copy_args_wire_input_and_template() {
    let input = PathBuf::from("/tmp/in/normalized.mp3");
    let template = PathBuf::from("/tmp/out/segment-%03d.mp3");
    let args = build_segment_copy_args(&input, 5, &template);
    let as_str = to_strings(&args);

    assert!(
        window_has(&as_str, "-i", "/tmp/in/normalized.mp3"),
        "入力ファイルが -i で渡されていません: {as_str:?}"
    );
    assert_eq!(
        as_str.last().map(String::as_str),
        Some("/tmp/out/segment-%03d.mp3"),
        "出力 segment テンプレートが末尾に来ていません: {as_str:?}"
    );
    // split_minutes=5 → 300 秒境界。
    assert!(window_has(&as_str, "-segment_time", "300"));
}
