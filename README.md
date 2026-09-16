<p align="center">
  <img src="assets/logo.png" width="88" alt="orange">
</p>

<h1 align="center">orange</h1>

<p align="center">
  Share your game with friends, live, straight from your PC.<br>
  Mostre seu jogo para os amigos, ao vivo, direto do seu PC.
</p>

<p align="center">
  <a href="https://orangealpha0d8d5893e69a3.z15.web.core.windows.net/"><strong>Download for Windows</strong></a><br>
  Windows 10 / 11, 64-bit · Free beta
</p>

![Orange streaming a game to two viewers](website/screenshots/streaming.png)

Orange streams a game window (picture **and** game sound) directly from the
host PC to each viewer. No server re-encode, no account needed to join:
the host sends a room code, the viewer pastes it, done.

> 🇧🇷 Versão em português abaixo: [Guia rápido em português](#guia-rápido-em-português).

## Watch a stream

1. Install Orange (both sides need it).
2. Ask the host for the room code.
3. Open Orange → **Join with a code** → paste the code.

The stream opens in its own window. Close it any time; the host keeps going.

## Host a stream

1. Open Orange → **Start streaming**.
2. Pick a quality, then click your game's preview.
   It starts immediately.
3. Click the **Share code** to copy it and send it to your friends.
4. **Stop streaming** when you're done.

## Add friends (skip the codes)

Codes work fine, but adding friends means no pasting every time:

1. Both sides sign in with Discord.
2. Ask your friend for their personal friend code, click **Add friend**,
   check the profile, then **Send request**.
3. They accept in **Requests**. One accept adds both sides.
4. From then on, when a friend is live you just press **Join**.

A friend code is **not** a room code: the friend code adds the person,
the room code joins one specific stream.

## The one audio rule

- Share the **game window** → viewers hear the game only.
  Voice chat, music and notifications stay out.
- Share the **Full display** → viewers hear **everything**
  (it says so on the row: "includes all system audio"), including the
  voices of everyone in your call. Only use it when audio doesn't matter
  for the call.

## Good to know

- Each viewer gets their own stream from your PC, so your **upload** is
  the limit (roughly 18 Mbps per viewer at 1080p60; more viewers need
  more upload).
- Anyone holding the room code can join and can pass it on. The friends
  list decides who *discovers* your stream, not who can enter with the code.
- Needs a 64-bit Windows 10/11 PC with a hardware H.265 or H.264 encoder;
  keep your graphics drivers updated.
- The beta installer is not signed yet, so Windows SmartScreen warns about
  an unknown publisher. That warning is expected during the beta.
- Networks that require a TURN relay are not supported yet.

## If something doesn't work

1. Open **Settings → Troubleshooting → Troubleshoot** and run the checks.
   Green means fine, red means it needs attention.
2. If it offers **Fix connection**, accept it — Windows may ask for
   permission — then try the stream again.
3. Still stuck? **Send report** (you need to be signed in) or
   **Copy report** and share it with us. **Open folder** shows the logs
   from your recent sessions. Nothing is uploaded automatically.

---

## Guia rápido em português

**Assistir:** instale o Orange, peça o código da sala, abra o Orange →
**Join with a code** → cole o código.

**Transmitir:** abra o Orange → **Start streaming** → escolha a qualidade →
clique na janela do jogo (começa na hora) → clique no código da sala para
copiar e mande para os amigos. **Stop streaming** para encerrar.

**Amigos (sem código toda vez):** os dois entram com Discord. Peça o código
de amigo da pessoa, clique **Add friend**, confira o perfil e **Send
request**. Ela aceita em **Requests**. Depois é só apertar **Join** quando
ela estiver ao vivo. Código de amigo adiciona a pessoa; código de sala
entra numa transmissão específica.

**Regra de ouro do áudio:** compartilhe a **janela do jogo** — os viewers
ouvem só o jogo. A **tela cheia** puxa o som do sistema junto, incluindo a
voz do pessoal na call. Só use tela cheia se o áudio não importar.

**Bom saber:** cada viewer recebe um stream separado do seu PC, então seu
upload limita quanta gente assiste bem. Quem tiver o código entra e pode
repassar. Precisa de Windows 10/11 64-bit com encoder H.265/H.264 e driver
atualizado. O instalador beta não é assinado, então o SmartScreen avisa —
é esperado. Redes que exigem relay TURN não funcionam ainda.

**Deu problema?** Settings → Troubleshooting → Troubleshoot, use
**Fix connection** se oferecer, e **Send report** (logado) ou
**Copy report** se continuar.

---

## For developers

Orange is a Rust workspace: media CLI, GPUI desktop client, signalling
relay, updater. Start with [ARCHITECTURE.md](ARCHITECTURE.md) for process
boundaries and source maps, then [CONTRIBUTING.md](CONTRIBUTING.md) for
build, test and release instructions.

## License

MIT — see [LICENSE](LICENSE). The installer redistributes part of the
GStreamer runtime, dynamically linked and unmodified (see
[CONTRIBUTING.md](CONTRIBUTING.md#license-notes) for the third-party
licence details.
