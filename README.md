# RevMagic Runtime

`revmagic-runtime` converts supported URLs and local media to MP3. It provides
a CLI and a stdio MCP server.

Use only media that you are authorized to access and process. Respect the
terms and copyright rules of each source service.

## Requirements

- Rust 1.86.0 or newer
- `ffmpeg` and `ffprobe` on `PATH`
- `yt-dlp` on `PATH` for extractor-backed URLs

## Build and test

```bash
cargo build --locked --release -p revmagic-cli -p revmagic-mcp
cargo test --locked --workspace
```

## Install locally

```bash
git clone https://github.com/sasuketorii/revmagic-runtime.git
cd revmagic-runtime
cargo install --path crates/revmagic-cli --locked
cargo install --path crates/revmagic-mcp --locked
```

Ensure Cargo's user-local bin directory is on `PATH` before invoking either
binary.

## CLI

```bash
revmagic-cli url-to-mp3 --input ./recording.mp4 --output-dir ./output
revmagic-cli url-to-mp3 --url https://example.com/media.m3u8 --output-dir ./output
```

## MCP server

```bash
revmagic-mcp
```

The MCP server communicates over standard input/output using JSON-RPC. Keep
stdout reserved for protocol messages.

Register the installed binary with either client:

```bash
codex mcp add revmagic -- revmagic-mcp
claude mcp add --scope user revmagic -- revmagic-mcp
```

For a site that requires authentication, provide only credentials you are
authorized to use through the `cookie` and `referer` arguments of `url_to_mp3`.
The runtime does not acquire browser sessions or start a companion application;
without explicit values it fails closed and returns retry guidance.

## License

MIT. See [LICENSE](LICENSE).
