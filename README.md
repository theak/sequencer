# AK-16 Sequencer

An 808-style drum machine and synth sequencer that runs in your browser. It has 16-step patterns with up to 4 pages, eight drums with a tone knob each, pitched drums (808, tom, conga, cowbell) played from a keyboard, mono and polyphonic (chord) synth channels, swing, sidechain and tape warble. You can save beats (⌘S), see at a glance whether you have unsaved changes, flip Compare to hear the last save before you overwrite it, and revert to it in one click. There's a live spectrogram of the loop, and you can export the beat as a WAV. When the AI builds on your current beat, it gets that spectrogram too, so it can hear the mix and not just read the notes. You can also describe a groove and have an AI model write it for you.

It's one small Rust binary in a `scratch` container, with the whole frontend baked in. Beats are saved as JSON files on a volume. AI generation goes through [OpenRouter](https://openrouter.ai), so the API key stays on the server.

## Quick start with Docker

```bash
docker run -p 42716:42716 \
  -v sequencer-data:/data \
  -e OPENROUTER_API_KEY="sk-or-..." \
  akshaykannan/sequencer
```

Visit `http://localhost:42716`.

## Or use Docker Compose

`docker-compose.yml`:

```yaml
services:
  sequencer:
    image: akshaykannan/sequencer
    ports:
      - "42716:42716"
    environment:
      - OPENROUTER_API_KEY=${OPENROUTER_API_KEY:-}
    volumes:
      - sequencer-data:/data
    restart: unless-stopped

volumes:
  sequencer-data:
```

`.env` next to it:

```bash
OPENROUTER_API_KEY=sk-or-...
```

Then run `docker compose up -d`.

## Environment variables

| Variable | Default | What it does |
| --- | --- | --- |
| `OPENROUTER_API_KEY` | unset | Turns on AI beat generation. Without it, everything else still works. |
| `OPENROUTER_MODELS` | Sonnet, Haiku, GPT Luna, Gemini Flash | The models offered in the picker, as comma-separated `slug=Label` pairs. Only these can be requested. |
| `DATA_DIR` | `/data` | Where saved beats live. |
| `PORT` | `42716` | Port to listen on. |

The default models use OpenRouter's `~…-latest` aliases (`~anthropic/claude-sonnet-latest`, `~anthropic/claude-haiku-latest`, `~openai/gpt-luna-latest` and `~google/gemini-flash-latest`), so they track the newest release in each family automatically.

## Storage

Each saved beat is one JSON file at `$DATA_DIR/<owner>/beats/<id>.json`. There's no database. To back up, copy the volume. Everything is under the `local` owner until sign-in exists (see Roadmap).

Your working beat also autosaves in the browser, so a refresh never loses it, even without the server's saved list.

There's no sign-in yet, so anyone who can reach the server can save and delete beats. Keep it on your LAN or put it behind a reverse proxy with auth if you expose it.

## Deploy to Google Cloud Run

Cloud Run can run the Docker Hub image directly. Mount a Cloud Storage bucket at `/data` so beats persist:

```bash
gcloud storage buckets create gs://YOUR-BUCKET --location=us-central1
printf 'sk-or-...' | gcloud secrets create openrouter-key --data-file=-

gcloud run deploy sequencer \
  --image=docker.io/akshaykannan/sequencer:latest \
  --region=us-central1 \
  --allow-unauthenticated \
  --execution-environment=gen2 \
  --max-instances=1 \
  --add-volume=name=data,type=cloud-storage,bucket=YOUR-BUCKET \
  --add-volume-mount=volume=data,mount-path=/data \
  --set-secrets=OPENROUTER_API_KEY=openrouter-key:latest
```

The service account needs `roles/storage.objectUser` on the bucket and `roles/secretmanager.secretAccessor` on the secret. `--max-instances=1` keeps a single writer on the bucket, which is plenty for personal use. Cloud Run sets `PORT` itself, and the app follows it.

## Roadmap: sign-in

The app stays fully usable signed out. Sign-in will only gate saving and AI generation. The server is already shaped for it:

- Every saved-beat call is scoped to an `Owner` (`src/store.rs`). Today it's always `local`. With sign-in, it becomes the Google account id from a session cookie, and beats land in `$DATA_DIR/<google-sub>/beats/`. There's no storage migration.
- The planned flow is Google Identity Services in the browser, then the server verifies the ID token and sets a signed HttpOnly cookie. An `AUTH_MODE=none|google` switch keeps today's behavior as the self-host default.
- `/api/generate` will take the same `Owner` and keep per-user usage counts for metering.
- `/api/config` already reports `auth`, `save` and `generate`, so the UI can adapt.

## Development

The server is Rust (axum). The frontend is a single HTML file, `static/index.html`, with plain JS and WebAudio and no build step.

```bash
DATA_DIR=./data cargo run      # http://localhost:42716
cargo test
docker build . -t sequencer
```

Pushes to `main` run the tests, then publish a multi-arch (`amd64` and `arm64`) `akshaykannan/sequencer:latest` to Docker Hub. This needs the `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` repo secrets.
