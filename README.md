# 16.AKS.ZONE

An 808-style drum machine and synth sequencer that runs in your browser, live at [16.aks.zone](https://16.aks.zone). It has 16-step patterns with up to 4 pages, eight drums with a tone knob each, pitched drums (808, tom, conga, cowbell) played from a keyboard, mono and polyphonic (chord) synth channels, YouTube samples that play along with the beat, swing, sidechain and tape warble.

The File menu has New, Open, Save (⌘S), Save as, Rename, Download JSON and Export audio (WAV). The save pill shows at a glance whether you have unsaved changes. Flip Compare to hear the last save before you overwrite it, and revert to it in one click. Share gives you a link to the beat. There's a live spectrogram of the loop. You can also describe a groove and have an AI model write it for you, and the AI can get that spectrogram too, so it can hear the mix and not just read the notes.

Everything works signed out, with beats saved in your browser. Signing in with Google keeps them in an account, gives each one a link like `16.aks.zone/you/1`, and turns on AI beats (10 a day by default).

It's one small Rust binary in a `scratch` container, with the whole frontend baked in. Accounts and beats live in one SQLite file on a volume. AI generation goes through [OpenRouter](https://openrouter.ai), so the API key stays on the server.

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
      - GOOGLE_CLIENT_ID=${GOOGLE_CLIENT_ID:-}
      - AI_DAILY_LIMIT=${AI_DAILY_LIMIT:-10}
    volumes:
      - sequencer-data:/data
    restart: unless-stopped

volumes:
  sequencer-data:
```

`.env` next to it:

```bash
OPENROUTER_API_KEY=sk-or-...
# leave this out to run without sign-in (see below)
GOOGLE_CLIENT_ID=1234-abc.apps.googleusercontent.com
```

Then run `docker compose up -d`.

## Updating

Every merge to `main` publishes a new `akshaykannan/sequencer:latest`. Pull it and recreate the container:

```sh
docker compose pull && docker compose up -d
```

To check which build you're running, open `/api/version`. Its `commit` should match the latest commit on `main`. The startup log prints it too.

## Environment variables

| Variable | Default | What it does |
| --- | --- | --- |
| `OPENROUTER_API_KEY` | unset | Turns on AI beat generation. Without it, everything else still works. |
| `OPENROUTER_MODELS` | Sonnet, Haiku, GPT Luna, Gemini Flash | The models offered in the picker, as comma-separated `slug=Label` pairs. Only these can be requested. |
| `OPENROUTER_PREMIUM_MODELS` | GPT Astra, Claude Opus | Bigger models offered only to accounts whose own AI limit is over 10 (see Accounts). Same `slug=Label` format; empty turns them off. |
| `OPENROUTER_SEARCH_MODELS` | Gemini Flash | Model slugs that can search the web while they write, through OpenRouter's web search tool. Comma-separated; empty turns search off. |
| `GOOGLE_CLIENT_ID` | unset | Turns on Google sign-in. Without it, there's no sign-in and everyone shares one `local` account (see below). |
| `AI_DAILY_LIMIT` | `10` | AI generations per account per day (UTC). Only applies with sign-in on. |
| `DATA_DIR` | `/data` | Where the database (`sequencer.db`) lives. |
| `PORT` | `42716` | Port to listen on. |

The default models use OpenRouter's `~…-latest` aliases (`~anthropic/claude-sonnet-latest`, `~anthropic/claude-haiku-latest`, `~openai/gpt-luna-latest` and `~google/gemini-flash-latest`), so they track the newest release in each family automatically. The premium ones are GPT Astra (`~openai/gpt-astra-latest`) and Claude Opus (`~anthropic/claude-opus-latest`).

Gemini Flash can look things up while it writes (a song you name, a genre's usual tempo). It uses OpenRouter's `openrouter:web_search` tool, so OpenRouter runs the searches with Google's own search, and the model decides when to search. Searches cost extra on your OpenRouter bill (a cent or two per request that searches). If OpenRouter won't run the tool for a request, the server sends it again without search.

## Automation

Turn a loop into a song. Right-click any knob, slider, M/S button or the warble button and pick **Add automation lane**, or use **+ Add lane** in the Automation tab. The tab has a lane per control over a song of 1 to 64 loops:
- **Editing:** Snap (off, beat, page or loop) applies to everything below. It starts on Page and your browser remembers what you pick.
  - Continuous lanes are points joined by straight lines. Click a lane to add a point, drag a point to move it, and right-click one to delete it.
  - For a jump, click just beside a point. On its right, the line leaves from the new value; on its left, the line arrives at the new value and then jumps.
  - On/off lanes (mute, solo, warble) paint: drag on empty steps to turn them on. Over a block the cursor becomes an eraser, and dragging there turns it off.
  - Right-click a lane for shapes like ramps, waves or every other loop.
  - Right-click the song ruler between two loops to insert loops there (1, 2, 4, 8 or any number, up to 64 loops in all). Every lane after that point moves later, and the gap holds the value each lane had arriving there, so ramps and jumps carry on unchanged after it.
- **Playback:** **Plays: Loop / Automation** next to Play picks what you hear. Loop plays the pattern with the knobs as set. Automation plays the song, and the knobs move with their lanes. A knob that follows a lane stays locked until you bypass or remove the lane.
  - The choice is saved with the beat. A beat opens on Loop unless it was saved on Automation, and adding a beat's first lane switches it to Automation.
  - The automation editor always plays the automation: opening it switches Play to Automation, and switching Play to Loop goes back to the loop editor. Automation playback works in either editor.
- **Keys:** L flips Plays between Loop and Automation, and A flips between the loop editor and the automation editor.
- **AI:** in the Automation tab, the AI writes lanes for the beat on screen ("build for 4 loops, drop on loop 5, fade out").
- **Export:** with Automation playback on, Export audio renders the whole song.

In the JSON, a song is a `song` object, with `"play": "automation"` when Play plays the song (left out, it plays the loop). Each lane is a list of `[at, value]` points, where `at` counts loops from the start (0 is the start of loop 1). Values ramp between points, two points at the same `at` make a jump, and mute, solo and warble take `true`/`false`:

```json
"song": {
  "loops": 8,
  "play": "automation",
  "lanes": {
    "synthTone": [[0, 0.14], [8, 0.64]],
    "synthReverb": [[0, 0.1], [5, 0.1], [6, 0.85], [7, 0.85], [7, 0.15]],
    "kick.mute": [[0, true], [2, false]],
    "synth2.volume": [[0, 0.05], [2, 0.65]]
  }
}
```

Lane names:
- Globals: `swing`, `synthTone`, `transpose` (cents), `warble`, `synthReverb`, `sidechain`, `drumDrive` and `master`.
- Rows: `<row>.volume|tone|mute|solo`. A row is a drum id, `bass808`, `tom`, `conga`, `cowbell`, `synth1`… or `sample1`…, in order, and `tone` is that row's knob.
- Group buttons: `drums|synths|samples.mute|solo`.

## YouTube samples

Under Synths there's a Samples section. Each sample is a YouTube video with a start time, a speed (YouTube's 0.25x to 2x) and a length in beats, or ∞ to keep playing across loops. Click steps in its row to choose where it starts. If the length is longer than the pattern, the video plays out in full while the pattern loops under it. For example, 16 beats on a one-bar pattern plays through four loops before it restarts.

Samples play in YouTube's own embedded player, not through the audio engine. So:

- **Loose timing.** They follow the beat loosely. Each player measures how late it starts and fires that much earlier next time, which usually lands hits within a few tens of milliseconds once it has warmed up. An ad, or slow buffering, can still throw off a hit.
- **No effects.** Swing timing applies, but the master effects, sidechain and spectrogram don't touch them.
- **Not exported.** They aren't in WAV exports. Saved beats and JSON keep their settings.
- **Visible player.** The video stays visible, as YouTube requires.
- **Some videos won't load.** Videos whose owners block embedding don't load, and the card says so.
- **Needs the internet.** The rest of the app runs offline. Samples don't.
- **Use the page's web address.** Open the app from its address (like `https://16.aks.zone`), not a saved file, or YouTube refuses to play.

## Accounts

With `GOOGLE_CLIENT_ID` set (how 16.aks.zone runs):

- **Signed out**, the whole app works. Save and Open use your browser's storage. Open says so, and that clearing the browser loses them.
- **Signing in** with Google the first time asks for a username, which can't change later. Any beats saved in the browser move into the account, after the ones already there. That happens on every sign-in, so a new browser's beats get added too.
- **Signed in**, beats save to the account. Each one gets the next number, and its own link at `/<username>/<n>`. Numbers are never reused, so an old link never opens a different beat.
- **Share** gives that link (saving first if needed). Signed out, it packs the whole beat into a long link instead, and suggests signing up.
- **Links are public.** Anyone with a link can open the beat, play it and save their own copy. The numbers count up, so treat beats as public.
- **AI** needs an account, with `AI_DAILY_LIMIT` generations per day. A generation that fails before the model writes anything doesn't count.

To give one account its own AI limit:

```sh
docker compose exec sequencer /sequencer set-ai-limit akshay 1000
docker compose exec sequencer /sequencer set-ai-limit akshay default
```

An account whose own limit is over 10 also gets the premium models (`OPENROUTER_PREMIUM_MODELS`, GPT Astra and Claude Opus by default) in its model picker. Changing `ai_limit` in the `users` table directly works too, and it applies on the next page load, with no restart. That also holds without sign-in, for the `local` account.

Without `GOOGLE_CLIENT_ID`, there's no sign-in. Everyone is one built-in `local` account that saves to the server, with unmetered AI. That's fine on your own network, but don't expose it to the internet like that.

### Setting up Google sign-in

1. In the [Google Cloud console](https://console.cloud.google.com/apis/credentials), set up the OAuth consent screen (External, app name, support email).
2. Create credentials, then OAuth client ID, then **Web application**.
3. Under **Authorized JavaScript origins**, add `https://16.aks.zone` (and `http://localhost:42716` to try it locally). No redirect URIs are needed.
4. Put the client ID in `GOOGLE_CLIENT_ID`. It isn't a secret, and there's no client secret to keep.

The browser gets an ID token from Google. The server checks its signature against Google's public keys, its issuer, audience and expiry, then sets a 60-day HttpOnly session cookie. The cookie is `Secure`, so outside `localhost` sign-in needs HTTPS.

## Storage

Everything lives in one SQLite file, `$DATA_DIR/sequencer.db`: accounts, sessions, beats and AI usage. To back it up, copy the volume while the container is stopped, or take a live copy with `sqlite3 sequencer.db ".backup backup.db"`.

Your working beat also autosaves in the browser, so a refresh never loses it.

## Deploying publicly

The server speaks plain HTTP, so put something in front of it that handles HTTPS. A [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/) (`cloudflared`) works well and needs no open ports. Point the hostname's ingress at the app:

```yaml
ingress:
  - hostname: 16.aks.zone
    service: http://localhost:42716
  - service: http_status:404
```

The browser sees HTTPS, so the `Secure` session cookie works. Add `https://16.aks.zone` to the Google client's authorized JavaScript origins.

SQLite needs a real disk, so a network-mounted bucket (like Cloud Run with Cloud Storage) isn't a safe place for `DATA_DIR`. Stick to one instance with a persistent disk.

## Development

The server is Rust (axum). The frontend is a single HTML file, `static/index.html`, with plain JS and WebAudio and no build step.

```bash
DATA_DIR=./data cargo run      # http://localhost:42716
cargo test
docker build . -t sequencer
```

Pushes to `main` run the tests, then publish a multi-arch (`amd64` and `arm64`) `akshaykannan/sequencer:latest` to Docker Hub. This needs the `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` repo secrets.
