//! Speed-3: 直リンク URL + 分割指定 (`convert_url_stream_to_mp3_with_cancel`) の
//! 二重エンコード解消を **実際の呼び出しフロー** で証明する。
//!
//! ## 何を証明するか
//! 旧実装ではストリーム変換で正規化済み mp3 (`work_mp3`) を作った後、分割段で
//! `run_split_conversion` (libmp3lame 再エンコード) をかけていた (= 二重エンコード)。
//! 本テストは fake ffmpeg を実際に2回起動させ、その argv を記録することで:
//!
//! 1. ffmpeg が **正確に2回** 呼ばれること (1回目: ストリーム変換, 2回目: 分割)。
//! 2. **2回目の呼び出しに `-c copy` が含まれ、`libmp3lame` / `-q:a` を含まないこと**
//!    (= 分割段が再エンコードでなく stream copy であること)。
//! 3. 実際に `segment-*.mp3` が生成され、`ConversionSummary` に反映されること。
//!
//! を deterministic に検証する (実ネット / 実 ffmpeg 不要)。

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use revmagic_core::{convert_url_stream_to_mp3_with_cancel, BinaryResolver, Progress};

struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "revmagic-direct-split-{tag}-{}-{}",
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

struct FakeFfmpegResolver {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl BinaryResolver for FakeFfmpegResolver {
    fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
        match binary {
            "ffmpeg" => Ok(self.ffmpeg.clone()),
            "ffprobe" => Ok(self.ffprobe.clone()),
            other => Err(format!("not provided in test: {other}")),
        }
    }
}

/// 固定 duration (1200秒 = 20分) を返す fake ffprobe。
fn write_fake_ffprobe(dir: &Path) -> PathBuf {
    let path = dir.join("ffprobe");
    fs::write(&path, "#!/usr/bin/env bash\necho 1200.0\nexit 0\n").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// 呼ばれるたびに argv を `calls_dir/call-N.args` へ記録し、以下を模す fake ffmpeg:
/// - argv に `-f segment` が **無い** (= ストリーム変換段) 場合、最後の引数 (出力
///   mp3 パス) へダミーデータを書く。
/// - argv に `-f segment` が **ある** (= 分割段) 場合、`-segment_time` の直後の値を
///   秒数として読み、duration (1200) をその秒数で割った数だけ `segment-NNN.mp3`
///   (最後の引数のテンプレートを `printf` 展開) を生成する。
fn write_recording_ffmpeg(dir: &Path, calls_dir: &Path) -> PathBuf {
    let path = dir.join("ffmpeg");
    let calls_dir = calls_dir.to_string_lossy();
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
call_index_file="{calls_dir}/next_index"
if [[ -f "$call_index_file" ]]; then
  idx=$(cat "$call_index_file")
else
  idx=0
fi
echo $((idx + 1)) > "$call_index_file"
argfile="{calls_dir}/call-${{idx}}.args"
: > "$argfile"
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$argfile"
done

is_segment=false
segment_time=""
prev=""
for arg in "$@"; do
  if [[ "$prev" == "-f" && "$arg" == "segment" ]]; then
    is_segment=true
  fi
  if [[ "$prev" == "-segment_time" ]]; then
    segment_time="$arg"
  fi
  prev="$arg"
done

last="${{@: -1}}"
if [[ "$is_segment" == "true" ]]; then
  duration=1200
  seg_count=$(( (duration + segment_time - 1) / segment_time ))
  for ((i = 0; i < seg_count; i++)); do
    out=$(printf "$last" "$i")
    printf 'fake-segment-audio-%d' "$i" > "$out"
  done
else
  printf 'fake-mp3-audio' > "$last"
fi
echo "progress=end"
exit 0
"#
    );
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

fn noop_progress() -> impl Fn(&Progress) -> Result<(), String> {
    |_p: &Progress| Ok(())
}

#[test]
fn direct_url_split_uses_stream_copy_not_reencode() {
    let bin_dir = TmpDir::new("bin");
    let calls_dir = TmpDir::new("calls");
    let out_dir = TmpDir::new("out");

    let ffprobe = write_fake_ffprobe(&bin_dir.path);
    let ffmpeg = write_recording_ffmpeg(&bin_dir.path, &calls_dir.path);
    let resolver = FakeFfmpegResolver { ffmpeg, ffprobe };
    let progress = noop_progress();

    let summary = convert_url_stream_to_mp3_with_cancel(
        &resolver,
        "https://example.com/video.mp4",
        &out_dir.path,
        Some(10), // 10分ごとに分割 -> 1200秒 / 600秒 = 2セグメント
        false,
        None,
        None,
        None,
        &progress,
        None,
    )
    .expect("直リンク分割変換は成功するはず");

    // 1. ffmpeg がちょうど2回呼ばれている (ストリーム変換 + 分割)。
    let mut call_files: Vec<PathBuf> = fs::read_dir(&calls_dir.path)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("args"))
        .collect();
    call_files.sort();
    assert_eq!(
        call_files.len(),
        2,
        "ffmpeg 呼び出し回数がストリーム変換+分割の2回であるべき: {call_files:?}"
    );

    // 2. 2回目 (分割段) の argv に -c copy が含まれ、libmp3lame / -q:a を含まない。
    let split_args = fs::read_to_string(&call_files[1]).unwrap();
    let split_lines: Vec<&str> = split_args.lines().collect();
    assert!(
        split_lines.windows(2).any(|w| w == ["-c", "copy"]),
        "分割段の argv に `-c copy` が含まれていません: {split_lines:?}"
    );
    assert!(
    !split_lines.contains(&"libmp3lame"),
    "分割段の argv に libmp3lame (再エンコード) が含まれています (二重エンコード): {split_lines:?}"
  );
    assert!(
    !split_lines.contains(&"-q:a"),
    "分割段の argv に -q:a (再エンコード品質指定) が含まれています (二重エンコード): {split_lines:?}"
  );
    assert!(
        split_lines.windows(2).any(|w| w == ["-f", "segment"]),
        "分割段の argv に segment muxer が指定されていません: {split_lines:?}"
    );

    // 3. 1回目 (ストリーム変換段) の argv は URL を直接 -i に渡し、正規化引数
    //    (libmp3lame) を含む (= 単一パスの正規化エンコードは維持されている)。
    let stream_args = fs::read_to_string(&call_files[0]).unwrap();
    let stream_lines: Vec<&str> = stream_args.lines().collect();
    assert!(
    stream_lines.contains(&"libmp3lame"),
    "ストリーム変換段の argv に libmp3lame が含まれていません (正規化エンコードが消えている): {stream_lines:?}"
  );
    assert!(
        stream_lines
            .windows(2)
            .any(|w| w == ["-i", "https://example.com/video.mp4"]),
        "ストリーム変換段の argv が URL を直接 -i していません: {stream_lines:?}"
    );

    // 4. 成果物として segment-*.mp3 が実際に生成され summary に反映されている。
    assert_eq!(
        summary.file_count, 2,
        "1200秒 / 600秒 = 2セグメントが生成されるはず"
    );
    assert!(summary.files.iter().all(|f| f.path.contains("segment-")));
    // 作業用の *.streamed.mp3 は削除され、成果物として残らない。
    assert!(
        !summary.files.iter().any(|f| f.path.contains("streamed")),
        "作業用の streamed.mp3 が成果物に残っています: {:?}",
        summary.files
    );
}

/// [手動検証用・実ネットワーク] 実際の Apple 公式サンプル HLS ストリームを
/// `convert_url_stream_to_mp3_with_cancel` に直接渡し、Speed-4 (HLS 直接MP3化)
/// が実機の ffmpeg / 実ネットワークで動くことを検証する。
///
/// GUI (computer-use) を操作せずに検証するためのテスト。実ネットワーク依存の
/// ため通常の `cargo test` には含めず、`cargo test -- --ignored` で明示実行する。
#[test]
#[ignore]
fn real_hls_stream_converts_directly_to_mp3() {
    use revmagic_core::FallbackBinaryResolver;

    let resolver = FallbackBinaryResolver::new();

    let out_dir = TmpDir::new("real-hls");
    let progress = noop_progress();

    let summary = convert_url_stream_to_mp3_with_cancel(
    &resolver,
    "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_4x3/bipbop_4x3_variant.m3u8",
    &out_dir.path,
    None, // 非分割: HLS 直接MP3化 (Speed-4) の単一パスを検証する。
    false,
    None,
    None,
    None,
    &progress,
    None,
  )
  .expect("実HLSストリームの直接MP3化は成功するはず");

    assert_eq!(summary.file_count, 1);
    let path = PathBuf::from(&summary.files[0].path);
    assert!(path.exists(), "生成された mp3 が存在しません: {path:?}");
    let bytes = fs::metadata(&path).unwrap().len();
    assert!(
        bytes > 10_000,
        "mp3 ファイルサイズが小さすぎます (壊れた出力の疑い): {bytes} bytes"
    );
}
