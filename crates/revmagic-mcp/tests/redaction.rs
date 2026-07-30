//! 回帰テスト (dual-xhigh blocking 修正): 署名付き / 期限付き URL の query token
//! が MCP の JSON-RPC 応答 (stdout) や診断ログ (stderr) に **生のまま流出しない**
//! ことを、ビルド済みバイナリへ実際に JSON-RPC を流して検証する。
//!
//! - probe_url: `?token=...&sig=...` を渡し、stdout に sentinel が残らない。
//! - url_to_mp3 (direct failure): 到達不能ポートで接続失敗させ、reqwest エラーが
//!   full URL を文字列化しても sentinel が stdout / stderr に残らない。
//! - cookie / referer sentinel も 0 件 (Slice 3 の redaction が回帰していない)。

use std::io::Write;
use std::process::{Command, Stdio};

/// URL クエリトークンの sentinel (probe_url 経路)。
const URL_TOKEN_SENTINEL: &str = "REV_MAGIC_URL_TOKEN_SENTINEL";
/// URL クエリトークンの sentinel (url_to_mp3 direct failure 経路)。
const URL_TOKEN_SENTINEL_DIRECT: &str = "REV_MAGIC_URL_TOKEN_SENTINEL_DIRECT";
/// cookie / referer 値の sentinel (Slice 3 redaction 回帰チェック)。
const COOKIE_SENTINEL: &str = "REV_MAGIC_COOKIE_SENTINEL";
const REFERER_SENTINEL: &str = "REV_MAGIC_REFERER_SENTINEL";

/// 改行区切り JSON-RPC 行を stdin から流し、(stdout, stderr) を返す。
fn run_mcp(lines: &[&str]) -> (String, String) {
    let exe = env!("CARGO_BIN_EXE_revmagic-mcp");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn revmagic-mcp");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        for line in lines {
            writeln!(stdin, "{line}").expect("write line");
        }
        // stdin を閉じてサーバループを終了させる。
    }

    let output = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn probe_url_does_not_leak_query_token_to_stdout() {
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"probe_url","arguments":{{"url":"https://example.com/clip.mp4?token={URL_TOKEN_SENTINEL}&sig=ABC"}}}}}}"#
    );
    let (stdout, stderr) = run_mcp(&[&req]);

    // 応答自体は届いている (route 判定が structuredContent に入る)。
    assert!(
        stdout.contains("\"route\":\"direct\""),
        "stdout missing route: {stdout}"
    );

    // sentinel が stdout (structuredContent + content[0].text) に残らない。
    assert!(
        !stdout.contains(URL_TOKEN_SENTINEL),
        "URL token sentinel leaked into stdout: {stdout}"
    );
    assert!(
        !stdout.contains("sig=ABC"),
        "sig leaked into stdout: {stdout}"
    );
    // stderr にも残らない。
    assert!(
        !stderr.contains(URL_TOKEN_SENTINEL),
        "URL token sentinel leaked into stderr: {stderr}"
    );
    assert!(
        !stderr.contains("sig=ABC"),
        "sig leaked into stderr: {stderr}"
    );
}

#[test]
fn url_to_mp3_direct_failure_does_not_leak_query_token() {
    // 既存ディレクトリが必要 (output_dir チェックを通過させ、接続失敗まで到達させる)。
    let out_dir = std::env::temp_dir();
    let out_dir = out_dir.to_string_lossy();

    // 到達不能ポート (port 9 = discard, 接続拒否) で direct download を失敗させる。
    // cookie / referer も渡し、Slice 3 の redaction が回帰していないことも確認する。
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"url_to_mp3","arguments":{{"url":"https://127.0.0.1:9/clip.mp4?token={URL_TOKEN_SENTINEL_DIRECT}&sig=XYZ","output_dir":"{out_dir}","cookie":"session={COOKIE_SENTINEL}","referer":"https://{REFERER_SENTINEL}.example.com/"}}}}}}"#
    );
    let (stdout, stderr) = run_mcp(&[&req]);

    // tool error として返っている (protocol error ではない)。
    assert!(
        stdout.contains("\"isError\":true"),
        "expected tool error: {stdout}"
    );

    for sentinel in [
        URL_TOKEN_SENTINEL_DIRECT,
        COOKIE_SENTINEL,
        REFERER_SENTINEL,
        "sig=XYZ",
    ] {
        assert!(
            !stdout.contains(sentinel),
            "sentinel '{sentinel}' leaked into stdout: {stdout}"
        );
        assert!(
            !stderr.contains(sentinel),
            "sentinel '{sentinel}' leaked into stderr: {stderr}"
        );
    }
}
