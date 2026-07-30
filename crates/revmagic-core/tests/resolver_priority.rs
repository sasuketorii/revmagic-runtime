//! Slice F1: resolver 安全化の回帰テスト。
//!
//! `fallback_resolve_binary` が yt-dlp の解決で **壊れた / 極小の text-script
//! shim や 0-byte placeholder を実体として優先採用しない** こと、そして
//! ffmpeg / ffprobe の既存解決 (極小 shebang script でも採用) を回帰させない
//! ことを検証する。実ネット・実 yt-dlp 非依存。
//!
//! 注: `fallback_resolve_binary` は resource_dirs を最優先したのち system path
//! (`/opt/homebrew` 等) と `which` を末尾に積む。ホスト依存を避けるため、本
//! テストは「resource_dirs 内での相対的な採用順序」のみを assert する
//! (実体を shim より前段の resource_dir に置けば、system path に到達せず確定する)。

#[cfg(unix)]
use std::{fs, path::PathBuf};

#[cfg(unix)]
use revmagic_core::{fallback_resolve_binary, resolve_binary_inner};

#[cfg(unix)]
fn unique_base(tag: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "revmagic-resolver-priority-{tag}-{}-{}",
        std::process::id(),
        nanos
    ))
}

#[cfg(unix)]
fn write_exec(path: &PathBuf, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// shim 相当: shebang で始まる極小 text-script (Homebrew launcher shim を模す)。
#[cfg(unix)]
const SHIM_BYTES: &[u8] = b"#!/opt/homebrew/Cellar/yt-dlp/x/libexec/bin/python\nimport sys\n";

/// 実体相当: self-contained binary を模した閾値超の非 shebang バイト列。
/// (resolver の YTDLP_SHIM_MAX_BYTES=64KiB を超える)
#[cfg(unix)]
fn real_binary_bytes() -> Vec<u8> {
    vec![0x7fu8; 64 * 1024 + 16]
}

/// yt-dlp: 前段 resource_dir の text-script shim を skip し、後段の
/// self-contained 実体を採用する。
#[cfg(unix)]
#[test]
fn ytdlp_skips_text_shim_then_takes_real_binary() {
    let base = unique_base("ytdlp-shim");

    let shim_dir = base.join("a");
    let shim = shim_dir.join("binaries").join("yt-dlp");
    write_exec(&shim, SHIM_BYTES);

    let real_dir = base.join("b");
    let real = real_dir.join("binaries").join("yt-dlp");
    write_exec(&real, &real_binary_bytes());

    let resolved = fallback_resolve_binary("yt-dlp", &[shim_dir.clone(), real_dir.clone()])
        .expect("resolver should fall through past the shim to the real binary");

    assert_eq!(
        resolved, real,
        "must skip the text-script shim and pick the real binary"
    );
    assert_ne!(
        resolved, shim,
        "must not adopt the broken launcher shim as yt-dlp"
    );

    let _ = fs::remove_dir_all(&base);
}

/// yt-dlp: 0-byte placeholder を skip し、後段の実体を採用する。
#[cfg(unix)]
#[test]
fn ytdlp_skips_zero_byte_placeholder_then_takes_real_binary() {
    let base = unique_base("ytdlp-zero");

    let ph_dir = base.join("a");
    let placeholder = ph_dir.join("binaries").join("yt-dlp");
    write_exec(&placeholder, b"");

    let real_dir = base.join("b");
    let real = real_dir.join("binaries").join("yt-dlp");
    write_exec(&real, &real_binary_bytes());

    let resolved = fallback_resolve_binary("yt-dlp", &[ph_dir.clone(), real_dir.clone()])
        .expect("resolver should fall through past the 0-byte placeholder");

    assert_eq!(
        resolved, real,
        "must skip 0-byte placeholder and pick the real binary"
    );
    assert_ne!(
        resolved, placeholder,
        "must not adopt the 0-byte placeholder"
    );

    let _ = fs::remove_dir_all(&base);
}

/// yt-dlp: shim が先・実体が後でも、本物が無ければ shim を採用しない
/// (前段に shim、後段にも shim のみ -> resource_dirs では採用されず、
///  system path / which へ fallthrough する)。
/// ここでは「shim だけを resource_dirs に置いたとき、その shim path が
/// 返らない」ことを assert する (system path 上の本物が無ければ Err、
/// 本物があればその本物 path)。いずれにせよ shim path は返らない。
#[cfg(unix)]
#[test]
fn ytdlp_never_returns_resource_dir_shim() {
    let base = unique_base("ytdlp-only-shim");

    let shim_dir = base.join("a");
    let shim = shim_dir.join("binaries").join("yt-dlp");
    write_exec(&shim, SHIM_BYTES);

    match fallback_resolve_binary("yt-dlp", std::slice::from_ref(&shim_dir)) {
        Ok(resolved) => assert_ne!(
            resolved, shim,
            "resolver must never adopt the resource-dir text-script shim as yt-dlp"
        ),
        Err(_) => { /* system path にも実体が無い -> Err は許容 (shim を掴んでいない) */
        }
    }

    let _ = fs::remove_dir_all(&base);
}

/// yt-dlp 回帰是正 (指摘1): **PATH (非bundled) 上の極小 shebang yt-dlp** は採用される。
///
/// `/opt/homebrew/bin/yt-dlp` の実体は 195 byte の Homebrew entry-point shim
/// (`#!.../python` 参照) だが `--version` は正常応答する。これは「その場で動く」
/// working shim なので resolver は弾いてはいけない。bundled 候補にだけ shim 拒否を
/// 適用し、SystemPath 候補は共通条件のみで受理することを deterministic に検証する。
///
/// ホスト依存を避けるため、`resolve_binary_inner` に **fake system dir** を渡し、
/// その配下 (`<dir>/yt-dlp`, PATH 同様に bin 直下) に極小 shebang script を置く。
/// resource_dirs は空なので bundled 候補は無く、SystemPath 候補だけで解決される。
#[cfg(unix)]
#[test]
fn ytdlp_adopts_small_shebang_path_candidate() {
    let base = unique_base("ytdlp-path-shebang");

    // PATH/Homebrew 相当の system dir。候補は <dir>/yt-dlp (bin 直下)。
    let system_dir = base.join("opt-bin");
    let path_ytdlp = system_dir.join("yt-dlp");
    // working Homebrew entry-point shim を模す極小 shebang script。
    write_exec(&path_ytdlp, SHIM_BYTES);

    // local_dirs / resource_dirs (bundled) は空。system_dirs にだけ working shim を置く。
    let resolved =
        resolve_binary_inner("yt-dlp", &[], &[], std::slice::from_ref(&system_dir), None)
            .expect("small-shebang PATH yt-dlp (working Homebrew entry-point) must be adopted");

    assert_eq!(
        resolved, path_ytdlp,
        "PATH (non-bundled) small-shebang yt-dlp must be accepted, not rejected as a broken shim"
    );

    let _ = fs::remove_dir_all(&base);
}

/// 指摘1 の両立確認: 同じ極小 shebang バイト列でも **bundled 候補なら skip** され、
/// **PATH 候補なら採用** される (origin で挙動が分かれる)。
#[cfg(unix)]
#[test]
fn ytdlp_shim_skipped_in_bundled_but_adopted_in_path() {
    let base = unique_base("ytdlp-origin-split");

    // bundled: <resource_dir>/binaries/yt-dlp に shim -> skip される。
    let bundled_dir = base.join("bundled");
    let bundled_shim = bundled_dir.join("binaries").join("yt-dlp");
    write_exec(&bundled_shim, SHIM_BYTES);

    // system: <system_dir>/yt-dlp に shim -> 採用される。
    let system_dir = base.join("sys-bin");
    let path_shim = system_dir.join("yt-dlp");
    write_exec(&path_shim, SHIM_BYTES);

    let resolved = resolve_binary_inner(
        "yt-dlp",
        &[],
        std::slice::from_ref(&bundled_dir),
        std::slice::from_ref(&system_dir),
        None,
    )
    .expect("must fall through bundled shim to the working PATH shim");

    assert_eq!(
        resolved, path_shim,
        "bundled shim must be skipped and the PATH shim adopted"
    );
    assert_ne!(resolved, bundled_shim, "bundled shim must not be adopted");

    let _ = fs::remove_dir_all(&base);
}

/// 指摘1 (working slot): 実行ファイル隣接の **local_dir** に置いた極小 shebang
/// yt-dlp は採用される (auth helper_no_leak の `current_exe().parent()/binaries/yt-dlp`
/// 注入と同じ経路)。bundled とは異なり shim 拒否しないことを保証する。
#[cfg(unix)]
#[test]
fn ytdlp_adopts_small_shebang_local_dir_candidate() {
    let base = unique_base("ytdlp-local-shebang");

    // local slot: <local_dir>/binaries/yt-dlp に極小 shebang script。
    let local_dir = base.join("exe-adjacent");
    let local_ytdlp = local_dir.join("binaries").join("yt-dlp");
    write_exec(&local_ytdlp, SHIM_BYTES);

    let resolved = resolve_binary_inner("yt-dlp", std::slice::from_ref(&local_dir), &[], &[], None)
        .expect("small-shebang yt-dlp in a working local_dir must be adopted");

    assert_eq!(
        resolved, local_ytdlp,
        "local_dir (working) small-shebang yt-dlp must be accepted, not rejected as a bundled shim"
    );

    let _ = fs::remove_dir_all(&base);
}

/// ffmpeg / ffprobe 回帰防止: yt-dlp 専用の shim 判定が ffmpeg/ffprobe には
/// 適用されず、極小 shebang script でも引き続き採用されること。
#[cfg(unix)]
#[test]
fn ffmpeg_ffprobe_still_resolve_small_shebang_scripts() {
    let base = unique_base("ffmpeg-regress");

    for name in ["ffmpeg", "ffprobe"] {
        let dir = base.join(name);
        let path = dir.join("binaries").join(name);
        // 極小 shebang script (yt-dlp なら弾かれるが ffmpeg/ffprobe では採用される)。
        write_exec(&path, b"#!/bin/sh\nexit 0\n");

        let resolved = fallback_resolve_binary(name, std::slice::from_ref(&dir))
            .unwrap_or_else(|e| panic!("bundled {name} should still resolve: {e}"));
        assert_eq!(
            resolved, path,
            "bundled {name} (small shebang) must still be picked"
        );
    }

    let _ = fs::remove_dir_all(&base);
}
