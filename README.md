# Footy

Footy is a simple local speech-to-text desktop app.

- Records from your microphone
- Transcribes locally with Parakeet through ONNX Runtime
- Uses Silero VAD to skip silence
- Pastes the transcript into the app you were using

## Development

Requirements: Rust and Bun.

```bash
bun install
bun run tauri dev
```

Build:

```bash
bun run tauri build
```

## CLI

```bash
footy --toggle-transcription
footy --toggle-post-process
footy --cancel
footy --start-hidden
footy --no-tray
```

## License

MIT. See [LICENSE](LICENSE).
