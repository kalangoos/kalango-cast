<p align="center">
  <img src="assets/logo-kalango-cast.png" alt="Kalango Cast" width="200">
</p>

# Kalango Cast

O estúdio de gravação e transmissão do [KalangoOS](https://github.com/kalangoos/kalangoos): nativo, em Rust, feito
para gastar o mínimo da máquina — a captura, a composição e a codificação ficam na GPU, sem cópias pela CPU.

Este repositório tem, por enquanto, o **motor** (fase 1): grava um monitor, uma janela ou uma área da tela, com o som
do computador e o microfone, num MP4 que sobrevive a quedas. A interface do estúdio (os desenhos estão no Design
System do KalangoOS, página "34 · Kalango Cast") vem nas próximas fases.

## Estado real (leia primeiro)

O código compila e passa no `clippy -D warnings` e nos testes de unidade. Pouco mais que isso foi **executado**:

| O quê | Estado |
|---|---|
| `cargo check`, `clippy --all-targets -D warnings`, `fmt` | passam |
| Testes de unidade (relógio, tipos, MP4, conversão, OpenH264 sem rede, serviço) | passam, no disco local |
| NVENC H.264 e HEVC numa RTX 3090, sem tela (teste `#[ignore]`, `KALANGO_CAST_GPU=1`) | **rodou**, com quadros sintéticos de memória compartilhada (não DMA-BUF): 1080p, ~130–150 quadros/s no total (esse total **inclui o tempo de CPU que o teste gasta preenchendo os quadros sintéticos**, e o envio de cada quadro da CPU para a GPU; não é o desempenho de uma gravação real), `encode()` p50 5–6 ms; as saídas foram conferidas só por estrutura (quadros não vazios, IDR, IDR forçado), **não decodificadas** |
| Captura do niri (ScreenCast, DMA-BUF), PipeWire, som, MP4 gravado de ponta a ponta | **nunca executado** (precisa do niri com o patch de `tests/niri/` e de uma tela) |
| Sincronia de áudio e vídeo, latência de ponta a ponta, uso de CPU/GPU em gravação real | **nunca medidos**; [`docs/DESEMPENHO.md`](docs/DESEMPENHO.md) descreve o método e traz só o que foi medido |
| VA-API (AMD, Intel), OpenH264 | só a detecção do VA-API existe; o OpenH264 não foi rodado (o download conferido tem teste `#[ignore]`, `KALANGO_CAST_OPENH264_NET=1`) |

Testes que dependem de máquina, rede ou compositor são `#[ignore]` e dizem a variável de ambiente que os liga:
`KALANGO_CAST_GPU` (NVENC), `KALANGO_CAST_OPENH264_NET` (download), `KALANGO_CAST_NIRI` (sessão completa).
A lista abaixo ("O que já faz") é o que o código **foi escrito para fazer**, não o que foi verificado.

## O que o código foi escrito para fazer

- **Captura pela API de ScreenCast do niri** (`org.gnome.Mutter.ScreenCast`, como o shell do KalangoOS faz): monitor,
  janela ou área. Os quadros chegam por **DMA-BUF com modificadores explícitos** (ficam na memória da GPU; o fd é
  importado como textura, nunca copiado), com memória compartilhada como reserva.
- **Codificação por hardware**: **NVENC** (NVIDIA, H.264 e HEVC), aberto em tempo de execução, lendo a textura da GPU.
  A VA-API (AMD, Intel) já é detectada; a codificação por ela vem na fase 2. O **OpenH264** da Cisco é a última
  reserva, só em software: é baixado da Cisco quando você pede (`kalango-cast openh264 download`) e conferido por
  SHA-256 — nunca vem junto do Kalango Cast.
- **Som**: o do computador e o do microfone, cada um numa faixa **Opus** dentro do mesmo MP4.
- **Um relógio só**: vídeo e som são marcados no mesmo relógio monotônico; o som corrige deriva e falhas sozinho.
  A sincronia é medida com uma claquete (um clarão e um bipe) — veja `docs/DESEMPENHO.md`.
- **MP4 fragmentado e seguro contra quedas**: um fragmento a cada ~2 s; se o computador desligar no meio, o arquivo
  toca até o último fragmento completo.
- **Serviço `kalango-castd`**: JSON-RPC num socket Unix privado (`$XDG_RUNTIME_DIR/kalango/cast.sock`, 0600, e só
  aceita processos da mesma pessoa), para o shell, o Kalango Audio e o `kalangod`. **Ser da mesma pessoa não é consentimento**: por padrão
  (`consent = "ask"`), `record.start` responde `consent_required` até que a pessoa aprove, no terminal onde o
  `kalango-castd` roda, com `kalango-cast approve CÓDIGO` (veja "Privacidade e consentimento").

## Privacidade e consentimento

- **Como a captura acontece**: pela API `org.gnome.Mutter.ScreenCast` do niri (D-Bus da sessão) e pelo PipeWire (som
  do computador e microfone). O Kalango Cast não usa o portal do xdg-desktop-portal.
- **Consentimento (feito)**: `consent = "ask"` (padrão) ou `"allow"` em `$XDG_CONFIG_HOME/kalango/cast.toml`. Com `ask`,
  um cliente do socket não começa a gravar sozinho: recebe `consent_required` (erro -32001); o código de uso único
  (válido 120 s, cancelado após 5 erros, só para aquele pedido) aparece **apenas** no terminal do `kalango-castd`
  (o `Hook` do daemon), e a pessoa roda `kalango-cast approve CÓDIGO`; o mesmo `record.start` passa uma vez. O
  `kalango-cast record` na linha de comando é a própria pessoa pedindo e não passa por isso.
- **Indicador (parcial)**: o daemon avisa início, fim e falha da gravação ao `Hook` (impresso no terminal do
  `kalango-castd`); **não há indicador na tela nem sinal D-Bus ainda**.
- **Arquivo de saída**: criado com `O_EXCL|O_NOFOLLOW` (modo 0600) numa pasta que seja sua e não seja um link; só
  termina em `.mp4`, sem `..`; `overwrite` só troca uma gravação nossa anterior (arquivo comum seu, com um só nome,
  vazio ou começando com `ftyp`) — nunca `.bashrc` nem o destino de um link.
- **O que NÃO está feito**: nada impede um programa **hostil da mesma pessoa** que leia o terminal ou a memória do
  daemon (ou edite o `cast.toml` para `allow`); não há diálogo do compositor por janela, nem registro de auditoria,
  nem limite de tempo ocioso nas conexões, nem aviso de que o microfone está aberto fora do daemon; as pastas
  *acima* da pasta de saída podem ser links; o conteúdo gravado não é cifrado.

## Usar

```sh
cargo build --release
target/release/kalango-cast sources                         # monitores e janelas
target/release/kalango-cast record --seconds 30 --out gravacao.mp4
target/release/kalango-cast record --window 12 --no-mic --out janela.mp4   # --overwrite troca uma gravação anterior
target/release/kalango-cast record --area DP-1:100,100,1280x720 --codec hevc --out area.mp4
target/release/kalango-cast caps                            # codificadores desta máquina

target/release/kalango-castd &                              # o serviço
target/release/kalango-cast rpc record.start '{"source":{"kind":"monitor","connector":"DP-1"},"path":"/tmp/a.mp4"}'   # consent_required: o código aparece no terminal do kalango-castd
target/release/kalango-cast approve CÓDIGO                  # a pessoa aprova; repita o record.start
target/release/kalango-cast rpc record.status
target/release/kalango-cast rpc record.stop
```

Métodos do serviço: `capabilities`, `sources.list`, `record.start` (`source`, `path`, e opcionais `encoder`,
`codec`, `fps`, `bitrate_kbps`, `desktop_audio`, `mic`, `overwrite`), `record.status` (quadros por segundo,
perdidos, codificador, latência, taxa de bits, CPU, GPU) e `record.stop`.

## Peças

| Crate | O quê |
|---|---|
| `cast-core` | O relógio de mídia, os tipos comuns, as traduções (`tr`) |
| `cast-capture` | ScreenCast do niri por D-Bus, fluxos de vídeo e som do PipeWire |
| `cast-encode` | NVENC (OpenGL ↔ NVENC), sonda da VA-API, OpenH264 (download conferido), Opus |
| `cast-mux` | O gravador de MP4 fragmentado, e um leitor para conferir arquivos |
| `cast-engine` | O motor, o serviço `kalango-castd` e a linha de comando `kalango-cast` |
| `cast-testkit` | Sessão niri aninhada, claquete, análise de sincronia, benchmark (só para testes) |

## Desenvolver

Precisa de `libpipewire-0.3-dev`, `libclang-dev` e `pkg-config` para compilar; `ffmpeg` para alguns testes. As regras
são as do KalangoOS (veja `AGENTS.md`). O teste de ponta a ponta com um niri de verdade está em
`crates/cast-testkit/tests/niri_session.rs` e o patch do niri que ele usa em `tests/niri/`.

## Licença

GPL-3.0-or-later (`LICENSE`). Avisos de terceiros em `NOTICE`. O logo é marca do KalangoOS.
