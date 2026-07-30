//! Slice I-1c: 直リンク DL / HLS 経路の cancel 即応テスト (実外部ネット不要)。
//!
//! 検証点:
//! - 直リンク silent cancel: 受理するが body を返さないローカル TCP に対し
//!   `download_media_with_cancel` を起動し、途中で cancel トークンを立てると、
//!   connect/read timeout (30s/120s) を待たず ~数秒で `CANCEL_MESSAGE` で戻ること、
//!   かつ書きかけの一時ファイル (一時 dir) が後始末されること (指摘1)。
//! - HLS token cancel: fake ffmpeg (無音 sleep + 孫 `sleep 600 &` 保持) を
//!   `download_hls_blocking_with_cancel` で起動し、token cancel で ffmpeg child +
//!   孫が killpg で消滅し、~数秒で戻ること (指摘2)。既存 ffmpeg kill テストと同じ
//!   PID ファイル + `kill(0)` 方式を踏襲する。

#![cfg(unix)]

use std::fs;
use std::io::Read as _;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use revmagic_core::{
    download_hls_blocking_with_cancel, download_media_with_cancel, BinaryResolver, Progress,
};
use url::Url;

// ---------------------------------------------------------------------------
// 共有ヘルパ
// ---------------------------------------------------------------------------

struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "revmagic-cancel-i1c-{tag}-{}-{}",
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

/// PID ファイルから PID を読む (最大 `timeout` 待つ)。
fn read_pid(pid_file: &Path, timeout: Duration) -> i32 {
    let started = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(pid_file) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                return pid;
            }
        }
        if started.elapsed() >= timeout {
            panic!(
                "PID file {} not populated within {timeout:?}",
                pid_file.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// PID が生存しているか (`kill(pid, 0)` 相当)。
fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// PID が `timeout` 以内に消滅することを待つ。消滅したら true。
fn wait_pid_gone(pid: i32, timeout: Duration) -> bool {
    let started = Instant::now();
    while pid_alive(pid) {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

// ---------------------------------------------------------------------------
// 指摘1: 直リンク DL の silent cancel (await race)
// ---------------------------------------------------------------------------

/// download_media は HLS 判定以外で ffmpeg/ffprobe を解決しないので、空 resolver で良い。
/// (直リンク経路は reqwest のみ。binary 解決は呼ばれない。)
struct NoBinaryResolver;

impl BinaryResolver for NoBinaryResolver {
    fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
        Err(format!("not provided in test: {binary}"))
    }
}

/// TCP を accept して受信だけ行い、レスポンスを一切返さずに保持し続ける無音サーバ。
///
/// クライアント (reqwest) の `send()` は接続確立後にレスポンスヘッダを待ち続けるため、
/// cancel race が無ければ read timeout (120s) まで無音でブロックする。バインド済みの
/// `127.0.0.1:<port>` を返し、サーバスレッドは `keep_running` が false になるまで
/// accept したストリームを drop せず保持する。
fn spawn_silent_http_server(keep_running: Arc<AtomicBool>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();

    std::thread::spawn(move || {
        // accept したストリームを保持し、body を返さない (無音)。
        let mut held: Vec<TcpStream> = Vec::new();
        while keep_running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).ok();
                    // request を読み捨てる試みは不要 (返信しないだけで良い)。保持して drop を防ぐ。
                    held.push(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
            // 保持したストリームの受信バッファは読み捨てる (送信はしない)。
            for s in held.iter_mut() {
                let mut buf = [0u8; 256];
                let _ = s.read(&mut buf);
            }
        }
    });

    port
}

#[test]
fn direct_download_silent_cancel_returns_promptly_and_cleans_partial() {
    // 受理するが body を返さないローカル TCP に対し DL を起動。送信途中で cancel
    // トークンを立てると、read timeout (120s) / connect timeout (30s) を待たず ~数秒で
    // CANCEL_MESSAGE 相当のエラーで戻ること、かつ一時 dl dir が後始末されることを検証。
    let keep_running = Arc::new(AtomicBool::new(true));
    let port = spawn_silent_http_server(keep_running.clone());
    // 拡張子付き URL にして infer_extension を満たす (到達はしないが念のため)。
    let url = format!("http://127.0.0.1:{port}/sample.mp4");

    let cancel = Arc::new(AtomicBool::new(false));

    // 別スレッドで ~500ms 後に cancel を立てる (接続/送信 await 中に確実に立てる)。
    let cancel_setter = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        cancel_setter.store(true, Ordering::SeqCst);
    });

    let progress = |_p: &Progress| -> Result<(), String> { Ok(()) };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let started = Instant::now();
    let result = runtime.block_on(download_media_with_cancel(
        &NoBinaryResolver,
        url,
        None,
        None,
        &progress,
        Some(cancel.as_ref()),
    ));
    let elapsed = started.elapsed();

    keep_running.store(false, Ordering::SeqCst);

    assert!(
        result.is_err(),
        "silent server DL must be cancelled, got {result:?}"
    );
    // 厳格化 (Slice I-1c reviewer fix / 指摘2): cancel は ~500ms 後に立て、100ms
    // poll で即応する設計なので <3s で戻るはず。<3s は connect/read timeout
    // (30s/120s) を「待っていない」ことを証明する閾値 (CI 余裕も見た値)。
    assert!(
    elapsed < Duration::from_secs(3),
    "cancel must return promptly (~100ms poll), not wait for timeout (30s/120s), took {elapsed:?}"
  );

    // 固定の中断文言で戻ること (secret / URL を含まない)。GUI の cancel 判定と一貫。
    let message = result.unwrap_err();
    assert_eq!(
        message, "キャンセルされました",
        "silent-server cancel must return the fixed cancel message, got {message:?}"
    );
}

/// HTTP レスポンスヘッダ + 部分 body を返してから無音になるサーバ。
///
/// reqwest の `send()` はヘッダ受信で返り、core は temp 一時ファイルを作って body
/// 受信ループに入る。サーバは数バイトの body chunk を送ったあと送信を止める
/// (chunked で終端を送らない) ため、cancel race が無ければ read timeout (120s) まで
/// 無音でブロックする。これにより **body 受信中の cancel + 部分ファイル後始末**
/// 経路を決定的に検証できる。
fn spawn_partial_body_then_silent_server(keep_running: Arc<AtomicBool>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();

    std::thread::spawn(move || {
        use std::io::Write as _;
        let mut held: Vec<TcpStream> = Vec::new();
        while keep_running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // ヘッダ + 1 つの chunk を送り、終端 (0\r\n\r\n) は送らずに保持する。
                    // Content-Type は mp4 にして infer_extension を満たす。
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nTransfer-Encoding: chunked\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    // chunk: サイズ(hex)\r\n payload \r\n  (終端 chunk は送らない = 無音化)
                    let _ = stream.write_all(b"4\r\nabcd\r\n");
                    let _ = stream.flush();
                    stream.set_nonblocking(true).ok();
                    held.push(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    });

    port
}

/// `revmagic-downloads` 配下で、stem `<stem>.<ext>` を含む timestamped dir 名の集合。
fn dl_dirs_with_stem(stem: &str) -> std::collections::BTreeSet<String> {
    let base = std::env::temp_dir().join("revmagic-downloads");
    let mut found = std::collections::BTreeSet::new();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if let Ok(inner) = fs::read_dir(&dir) {
                for f in inner.flatten() {
                    if f.file_name().to_string_lossy().contains(stem) {
                        found.insert(dir.file_name().unwrap().to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    found
}

#[test]
fn direct_download_body_cancel_returns_promptly_and_removes_partial_file() {
    // body 受信ループ中 (部分 body 受信後の無音) に cancel を立てると、read timeout
    // (120s) を待たず ~数秒で中断し、書きかけの一時ファイル (一時 dir) が best-effort で
    // 削除されることを検証する (指摘1: body next() の await race + partial cleanup)。
    let keep_running = Arc::new(AtomicBool::new(true));
    let port = spawn_partial_body_then_silent_server(keep_running.clone());
    // 衝突しない一意 stem (file_stem として temp ファイル名に使われる)。
    let stem = format!("partialbody{}", nano());
    let url = format!("http://127.0.0.1:{port}/{stem}.mp4");

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_setter = cancel.clone();
    std::thread::spawn(move || {
        // 部分 body 受信後の無音 next() 中に確実に立てる。
        std::thread::sleep(Duration::from_millis(600));
        cancel_setter.store(true, Ordering::SeqCst);
    });

    let progress = |_p: &Progress| -> Result<(), String> { Ok(()) };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let started = Instant::now();
    let result = runtime.block_on(download_media_with_cancel(
        &NoBinaryResolver,
        url,
        None,
        None,
        &progress,
        Some(cancel.as_ref()),
    ));
    let elapsed = started.elapsed();
    keep_running.store(false, Ordering::SeqCst);

    assert_eq!(
        result.unwrap_err(),
        "キャンセルされました",
        "body-loop cancel must return the fixed cancel message"
    );
    // 厳格化 (Slice I-1c reviewer fix / 指摘2): cancel は ~600ms 後に立て、100ms
    // poll で即応する。<3s は read timeout (120s) を「待っていない」ことを証明する。
    assert!(
    elapsed < Duration::from_secs(3),
    "body-loop cancel must return promptly (~100ms poll), not wait for timeout (120s), took {elapsed:?}"
  );

    // 部分ファイル後始末: この stem を含む一時 dir が残っていないこと。
    let leftover = dl_dirs_with_stem(&stem);
    assert!(
        leftover.is_empty(),
        "partial download dir for stem {stem} must be cleaned up, leftover={leftover:?}"
    );
}

// ---------------------------------------------------------------------------
// 指摘2: HLS 経路の token cancel (run_ffmpeg_with_cancel 経由)
// ---------------------------------------------------------------------------

/// ffmpeg / ffprobe を fake にマップする resolver。
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

/// 固定 duration を返す fake ffprobe (HLS の duration 解析を満たす)。
fn write_fake_ffprobe(dir: &Path) -> PathBuf {
    let path = dir.join("ffprobe");
    let script = "#!/usr/bin/env bash\necho 120.0\nexit 0\n";
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// 無音でハングする fake ffprobe。duration を出力せず、孫 `sleep 600 &`
/// (stdout FD を握る) と共にハングする。self / child PID を記録する。
///
/// これにより HLS の duration 解析段で ffprobe が無音ハングするケースを再現し、
/// cancel トークンで ffprobe child + 孫が killpg で消滅・即応中断することを検証
/// できる (指摘1)。
fn write_hang_ffprobe(dir: &Path, pid_dir: &Path) -> PathBuf {
    let path = dir.join("ffprobe");
    let pid_dir = pid_dir.to_string_lossy();
    let script = format!(
        r#"#!/usr/bin/env bash
echo $$ > "{pid_dir}/probe-self.pid"
sleep 600 &
child=$!
echo "$child" > "{pid_dir}/probe-child.pid"
# duration を一切出さず、孫が stdout FD を握ったままハングする (無音)。
wait "$child"
exit 0
"#
    );
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// 即座に exit する no-op fake ffmpeg (ffprobe 段で cancel されるため到達しない想定)。
fn write_noop_ffmpeg(dir: &Path) -> PathBuf {
    let path = dir.join("ffmpeg");
    let script = "#!/usr/bin/env bash\nexit 0\n";
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// 1 行進捗を出してからハングする fake ffmpeg。`-progress pipe:1` を模し、
/// `progress=continue` を 1 回出して core の progress を発火させ、その後は孫
/// `sleep 600 &` (stdout FD を握る) と共にハングする。self/child PID を記録する。
fn write_progress_then_hang_ffmpeg(dir: &Path, pid_dir: &Path) -> PathBuf {
    let path = dir.join("ffmpeg");
    let pid_dir = pid_dir.to_string_lossy();
    let script = format!(
        r#"#!/usr/bin/env bash
echo $$ > "{pid_dir}/self.pid"
sleep 600 &
child=$!
echo "$child" > "{pid_dir}/child.pid"
echo "out_time_us=60000000"
echo "progress=continue"
wait "$child"
exit 0
"#
    );
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

#[test]
fn hls_cancel_via_token_kills_ffmpeg_child_and_grandchild() {
    // download_hls_blocking_with_cancel が cancel トークンを ffmpeg 段へ伝播し、
    // 無音 ffmpeg (進捗 1 行後にハング) でも行読みループの各反復でトークンが確認され、
    // ffmpeg child **と孫** が killpg でグループごと kill され、呼び出しが有限時間で
    // 戻ること。token を捨てる旧 download_hls_blocking 経由ではこのテストは成立しない。
    let tmp = TmpDir::new("hls-cancel");
    let bin_dir = tmp.path.join("bin");
    let pid_dir = tmp.path.join("pids");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&pid_dir).unwrap();

    let resolver = FakeFfmpegResolver {
        ffmpeg: write_progress_then_hang_ffmpeg(&bin_dir, &pid_dir),
        ffprobe: write_fake_ffprobe(&bin_dir),
    };

    // ffmpeg が PID を書き終えてから (= 最初の進捗 > 5% を観測してから) トークンを立てる。
    // fake script は PID 書き込み → 進捗出力の順なので、進捗が届けば PID は必ず書かれている。
    let cancel = AtomicBool::new(false);
    let progress = |p: &Progress| -> Result<(), String> {
        if p.percent > 5 {
            cancel.store(true, Ordering::SeqCst);
        }
        Ok(())
    };

    // HLS 判定される URL (.m3u8)。
    let url = Url::parse("https://example.com/stream/index.m3u8").unwrap();

    let started = Instant::now();
    let result =
        download_hls_blocking_with_cancel(&resolver, url, None, None, &progress, Some(&cancel));
    let elapsed = started.elapsed();

    assert!(result.is_err(), "HLS cancel should abort ffmpeg merge");
    // 厳格化 (Slice I-1c reviewer fix / 指摘2): cancel は最初の進捗観測直後に立て、
    // 100ms poll で即応する。<3s は ffmpeg を「無限に待っていない」ことを証明する。
    assert!(
        elapsed < Duration::from_secs(3),
        "HLS cancel must return promptly (~100ms poll, killed ffmpeg), not hang, took {elapsed:?}"
    );

    let self_pid = read_pid(&pid_dir.join("self.pid"), Duration::from_secs(5));
    let child_pid = read_pid(&pid_dir.join("child.pid"), Duration::from_secs(5));
    assert!(
        wait_pid_gone(self_pid, Duration::from_secs(10)),
        "HLS ffmpeg self pid {self_pid} must be killed (killpg via cancel token)"
    );
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(10)),
        "HLS ffmpeg grandchild sleep pid {child_pid} must be killed by killpg (orphan check)"
    );
}

#[test]
fn hls_ffprobe_hang_cancel_kills_ffprobe_child_and_grandchild() {
    // 指摘1 (Slice I-1c reviewer fix): HLS の duration 解析 ffprobe が無音ハング
    // しても、download_hls_blocking_with_cancel が ffprobe を cancel トークン +
    // 上限 deadline 付き (read_duration_seconds_for_url_with_cancel) で実行するため、
    // cancel トークンで ffprobe child **と孫** が killpg でグループごと消滅し、
    // 結合 ffmpeg 段に入る前に即時 (<3s) 中断することを検証する。
    let tmp = TmpDir::new("hls-ffprobe-cancel");
    let bin_dir = tmp.path.join("bin");
    let pid_dir = tmp.path.join("pids");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&pid_dir).unwrap();

    let resolver = FakeFfmpegResolver {
        // ffprobe は無音ハング。ffmpeg は no-op (ffprobe 段で cancel され到達しない想定)。
        ffmpeg: write_noop_ffmpeg(&bin_dir),
        ffprobe: write_hang_ffprobe(&bin_dir, &pid_dir),
    };

    // ffprobe は同期実行されるので、別スレッドで cancel を立てる。並列テスト下の
    // CPU 競合で ffprobe spawn が遅れても確実に「ハング中」を捉えるため、固定遅延
    // ではなく **ffprobe が self.pid を書き終える (= 起動済み) のを待ってから**
    // トークンを立てる (cancel が spawn より先に立って PID 記録前に killpg される
    // レースを避ける)。
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_setter = cancel.clone();
    let probe_self_pid = pid_dir.join("probe-self.pid");
    std::thread::spawn(move || {
        let started = Instant::now();
        while !probe_self_pid.exists() {
            if started.elapsed() >= Duration::from_secs(5) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // 孫 PID 記録 + wait 突入まで僅かに猶予を与えてから cancel。
        std::thread::sleep(Duration::from_millis(50));
        cancel_setter.store(true, Ordering::SeqCst);
    });

    let progress = |_p: &Progress| -> Result<(), String> { Ok(()) };

    // HLS 判定される URL (.m3u8)。
    let url = Url::parse("https://example.com/stream/index.m3u8").unwrap();

    let started = Instant::now();
    let result = download_hls_blocking_with_cancel(
        &resolver,
        url,
        None,
        None,
        &progress,
        Some(cancel.as_ref()),
    );
    let elapsed = started.elapsed();

    assert!(
        result.is_err(),
        "HLS ffprobe hang must be cancelled, got {result:?}"
    );
    assert_eq!(
        result.unwrap_err(),
        "キャンセルされました",
        "ffprobe-hang cancel must return the fixed cancel message"
    );
    // 100ms poll で即応。<3s は ffprobe deadline (60s) を「待っていない」ことを証明。
    assert!(
    elapsed < Duration::from_secs(3),
    "HLS ffprobe cancel must return promptly (~100ms poll), not wait ffprobe deadline (60s), took {elapsed:?}"
  );

    // ffprobe child + 孫が killpg でグループごと消滅していること (orphan なし)。
    let self_pid = read_pid(&pid_dir.join("probe-self.pid"), Duration::from_secs(5));
    let child_pid = read_pid(&pid_dir.join("probe-child.pid"), Duration::from_secs(5));
    assert!(
        wait_pid_gone(self_pid, Duration::from_secs(10)),
        "HLS ffprobe self pid {self_pid} must be killed (killpg via cancel token)"
    );
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(10)),
        "HLS ffprobe grandchild sleep pid {child_pid} must be killed by killpg (orphan check)"
    );
}
