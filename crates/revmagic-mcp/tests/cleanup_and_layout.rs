//! MCP 変換経路の後始末 / 出力レイアウト回帰テスト (バグ#2 の MCP 経路是正)。
//!
//! ビルド済み `revmagic-mcp` バイナリを実際に起動し、`url_to_mp3` を 1 リクエスト
//! 流して挙動を観測する。実ネットワーク・実 ffmpeg / yt-dlp は使わず、`ToolContext::new()`
//! が **実行ファイル隣接 `binaries/<bin>` を最優先解決** する性質を使って fake バイナリ
//! (ffmpeg / ffprobe / yt-dlp) を注入する (sandbox test と同じ sandbox 手法)。
//!
//! 検証点 (FIX-1 / FIX-2):
//! - **direct cleanup (成功)**: 直リンク経路で変換成功時に DL 一時ファイルと専用
//!   一時ディレクトリ (`$TMPDIR/revmagic-downloads/<ts>/`) が残らないこと。
//! - **direct cleanup (失敗)**: 変換 (ffmpeg) 失敗時にも上記 temp が残らないこと
//!   (`cleanup_download_temp` が `?` の前に走るため成否いずれでも掃除される)。
//! - **yt-dlp split layout**: yt-dlp + split 経路で、出力 dir 直下に生 MP3 が散らからず
//!   (= raw job folder 残置なし)、summary の `output_folder` が実在フォルダを指すこと。

#![cfg(unix)]

use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 一時ディレクトリ (Drop で再帰削除)。
struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rmcl-{tag}-{}-{}",
            std::process::id(),
            nano() % 1_000_000
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create tmp dir");
        Self { path }
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn nano() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// MCP バイナリを sandbox にコピーし、隣に `binaries/` を作って返す。
///
/// `ToolContext::new()` は `current_exe().parent()/binaries/<bin>` を最優先解決候補に
/// するため、sandbox にコピーした MCP の隣の `binaries/` に置いた fake が必ず採用される。
fn sandbox_mcp(dir: &Path) -> (PathBuf, PathBuf) {
    let real_exe = env!("CARGO_BIN_EXE_revmagic-mcp");
    let exe = dir.join("revmagic-mcp");
    std::fs::copy(real_exe, &exe).expect("copy mcp binary into sandbox");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin_dir = dir.join("binaries");
    std::fs::create_dir_all(&bin_dir).unwrap();
    (exe, bin_dir)
}

/// 固定 duration を返す fake ffprobe。
fn write_fake_ffprobe(bin_dir: &Path) {
    write_script(
        &bin_dir.join("ffprobe"),
        "#!/usr/bin/env bash\necho 120.0\nexit 0\n",
    );
}

/// 最後の positional 引数 (出力パス / セグメントテンプレート) のファイルを作って成功
/// する fake ffmpeg。`%03d` テンプレートは `000` に展開する (split の segment-NNN.mp3)。
fn write_ok_ffmpeg(bin_dir: &Path) {
    write_script(
        &bin_dir.join("ffmpeg"),
        r#"#!/usr/bin/env bash
set -uo pipefail
out=""
for arg in "$@"; do out="$arg"; done
out="${out//%03d/000}"
: > "$out"
exit 0
"#,
    );
}

/// 何も出力せず非 0 終了する fake ffmpeg (変換失敗を模す)。
fn write_failing_ffmpeg(bin_dir: &Path) {
    write_script(
        &bin_dir.join("ffmpeg"),
        "#!/usr/bin/env bash\necho \"ERROR: synthetic ffmpeg failure\" 1>&2\nexit 1\n",
    );
}

/// `--output <dir>/...` のディレクトリ (= yt-dlp の job folder) に mp3 を作る fake yt-dlp。
fn write_ok_ytdlp(bin_dir: &Path) {
    write_script(
        &bin_dir.join("yt-dlp"),
        r#"#!/usr/bin/env bash
set -euo pipefail
output_template=""
prev=""
for arg in "$@"; do
  if [[ "$prev" == "--output" ]]; then output_template="$arg"; fi
  prev="$arg"
done
if [[ -n "$output_template" ]]; then
  out_dir="$(dirname "$output_template")"
  : > "$out_dir/mocktitle.mp3"
fi
exit 0
"#,
    );
}

/// 小さな本文を返す loopback HTTP サーバ (direct DL を成功させる)。`keep_running` が
/// false になるまで accept ループを回す。バインド済みポートを返す。
fn spawn_http_server(keep_running: Arc<AtomicBool>) -> u16 {
    use std::io::Read;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let body = b"FAKE-MEDIA-BODY-FOR-DIRECT-DOWNLOAD";
        while keep_running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // リクエストを読み捨て、固定本文で応答する (Content-Length 付き)。
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: video/mp4\r\nConnection: close\r\n\r\n",
            body.len()
          );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    port
}

/// sandbox MCP バイナリへ `url_to_mp3` を 1 リクエスト送り、stdout/stderr を回収する。
///
/// `tmpdir` を `TMPDIR` で向け、direct DL の `revmagic-downloads/` を隔離 dir 配下に
/// 作らせて観測可能にする。
fn call_url_to_mp3(
    exe: &Path,
    tmpdir: &Path,
    output_dir: &Path,
    url: &str,
    split_minutes: Option<u64>,
) -> Output {
    let split = match split_minutes {
        Some(n) => format!(r#","split_minutes":{n}"#),
        None => String::new(),
    };
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"url_to_mp3","arguments":{{"url":"{url}","output_dir":"{}"{split}}}}}}}"#,
        output_dir.to_string_lossy()
    );
    let mut child = Command::new(exe)
        .env("TMPDIR", tmpdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sandboxed revmagic-mcp");
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{req}").unwrap();
    }
    child.wait_with_output().expect("wait")
}

/// `<tmpdir>/revmagic-downloads` 配下に残った temp (ファイル / サブディレクトリ) を数える。
fn downloads_residue(tmpdir: &Path) -> Vec<PathBuf> {
    let base = tmpdir.join("revmagic-downloads");
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&base) {
        for e in entries.flatten() {
            found.push(e.path());
        }
    }
    found
}

#[test]
fn direct_success_cleans_download_temp() {
    // FIX-1: direct 経路で変換成功時に DL 一時ファイル + 専用一時ディレクトリが残らない。
    let sandbox = TmpDir::new("ds");
    let (exe, bin_dir) = sandbox_mcp(&sandbox.path);
    write_fake_ffprobe(&bin_dir);
    write_ok_ffmpeg(&bin_dir);

    let tmp = TmpDir::new("dt");
    let out = TmpDir::new("do");

    let keep = Arc::new(AtomicBool::new(true));
    let port = spawn_http_server(keep.clone());
    let url = format!("http://127.0.0.1:{port}/clip.mp4");

    let output = call_url_to_mp3(&exe, &tmp.path, &out.path, &url, None);
    keep.store(false, Ordering::SeqCst);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    // tool は成功 (isError なし / structuredContent あり)。
    assert!(
        !stdout.contains("\"isError\":true"),
        "direct success expected, got stdout:\n{stdout}"
    );
    // DL 一時ディレクトリ (revmagic-downloads/<ts>/) も生 .mp4 も残らない。
    let residue = downloads_residue(&tmp.path);
    assert!(
        residue.is_empty(),
        "download temp must be cleaned on success, residue: {residue:?}"
    );
}

#[test]
fn direct_failure_cleans_download_temp() {
    // FIX-1: direct 経路で変換 (ffmpeg) 失敗時にも DL 一時ファイル + 専用一時ディレクトリが
    // 残らない (cleanup_download_temp が `?` の前に走るため成否いずれでも掃除)。
    let sandbox = TmpDir::new("fs");
    let (exe, bin_dir) = sandbox_mcp(&sandbox.path);
    write_fake_ffprobe(&bin_dir);
    write_failing_ffmpeg(&bin_dir);

    let tmp = TmpDir::new("ft");
    let out = TmpDir::new("fo");

    let keep = Arc::new(AtomicBool::new(true));
    let port = spawn_http_server(keep.clone());
    let url = format!("http://127.0.0.1:{port}/clip.mp4");

    let output = call_url_to_mp3(&exe, &tmp.path, &out.path, &url, None);
    keep.store(false, Ordering::SeqCst);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    // tool は失敗 (ffmpeg 非 0)。エラーとして表面化していること。
    assert!(
        stdout.contains("\"isError\":true"),
        "ffmpeg failure must surface as tool error, got stdout:\n{stdout}"
    );
    // それでも DL 一時ファイル + 専用一時ディレクトリは残らない。
    let residue = downloads_residue(&tmp.path);
    assert!(
        residue.is_empty(),
        "download temp must be cleaned on failure too, residue: {residue:?}"
    );
}

#[test]
fn ytdlp_split_layout_has_no_stray_mp3_and_real_output_folder() {
    // FIX-2: yt-dlp + split 経路 (core extract_url_to_mp3 一本化) で、出力 dir 直下に生 MP3
    // が散らからず (raw job folder 残置なし)、summary.output_folder が実在フォルダを指す。
    let sandbox = TmpDir::new("ys");
    let (exe, bin_dir) = sandbox_mcp(&sandbox.path);
    write_fake_ffprobe(&bin_dir);
    write_ok_ffmpeg(&bin_dir);
    write_ok_ytdlp(&bin_dir);

    let tmp = TmpDir::new("yt");
    let out = TmpDir::new("yo");

    // Loom 風 URL は extractor (yt-dlp) 経路。
    let url = "https://www.loom.com/share/abcdef0123456789";
    let output = call_url_to_mp3(&exe, &tmp.path, &out.path, url, Some(10));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !stdout.contains("\"isError\":true"),
        "yt-dlp split expected success, stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // 出力 dir 直下に生 MP3 が無いこと (yt-dlp の中間生 MP3 = mocktitle.mp3 が散らからない)。
    let stray_mp3: Vec<PathBuf> = std::fs::read_dir(&out.path)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("mp3"))
        .collect();
    assert!(
        stray_mp3.is_empty(),
        "no raw mp3 may sit directly in the output dir, found: {stray_mp3:?}"
    );

    // summary.output_folder を tool 応答 (structuredContent) から取り出し、実在フォルダで
    // 出力 dir 配下にあること、その中に分割セグメントがあることを確認する。
    let resp: serde_json::Value = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("id").is_some() && v.get("result").is_some())
        .unwrap_or_else(|| panic!("no JSON-RPC result line in stdout:\n{stdout}"));
    let structured = &resp["result"]["structuredContent"];
    let output_folder = structured["outputFolder"]
        .as_str()
        .unwrap_or_else(|| panic!("outputFolder missing in structuredContent: {structured}"));
    let folder = PathBuf::from(output_folder);
    assert!(
        folder.is_dir(),
        "output_folder must be a real dir: {output_folder}"
    );
    assert_eq!(
        folder.parent().map(Path::to_path_buf),
        Some(out.path.clone()),
        "output_folder (job folder) must live directly under the output dir"
    );
    // job folder 内に少なくとも 1 つの segment mp3 がある。
    let segs: Vec<PathBuf> = std::fs::read_dir(&folder)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("mp3"))
        .collect();
    assert!(
        !segs.is_empty(),
        "split output folder must contain segment mp3s"
    );
}
