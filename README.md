<p align="center">
  <img src="assets/logo-kalango-cast.png" alt="Kalango Cast" width="200">
</p>

# Kalango Cast

> **Prévia técnica: o motor nunca rodou em uma sessão real.** Compila, passa nos testes de unidade e o NVENC foi
> exercitado numa RTX 3090 com quadros sintéticos; a captura do niri, o som, a sincronia e uma gravação de ponta a
> ponta **nunca foram executados**. Veja "Estado real" abaixo antes de confiar em qualquer coisa daqui. Licença:
> GPL-3.0-or-later (`LICENSE`); terceiros em `NOTICE`.

O estúdio de gravação e transmissão do [KalangoOS](https://github.com/kalangoos/kalangoos): nativo, em Rust, feito
para gastar o mínimo da máquina — a captura, a composição e a codificação ficam na GPU, sem cópias pela CPU.

Este repositório tem o **motor** (fase 1): grava um monitor, uma janela ou uma área da tela, com o som do computador
e o microfone, num MP4 que sobrevive a quedas; e uma **primeira janela** (`kalango-cast-app`, GTK4) que fala com o
serviço. A janela é só o começo do estúdio desenhado no Design System do KalangoOS (página "34 · Kalango Cast"): não
há cenas com várias fontes, mixer, transmissão nem Diretor IA ainda.

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

| **Janela `kalango-cast-app`** (GTK4) | **abre e funciona numa sessão niri aninhada do KalangoOS** (capturas em [`docs/capturas/`](docs/capturas/)); **sem captura**: o niri aninhado em que foi aberta não oferece o ScreenCast (`org.gnome.Mutter.DisplayConfig` ausente no barramento dele), então a janela mostrou, de verdade, "A captura não está disponível nesta sessão" e o botão Gravar ficou desligado. **Nenhuma gravação foi feita pela janela**, nem com o niri com o patch de `tests/niri/` |
| Consentimento pela janela | testado com o código do serviço de verdade (testes de unidade e de integração: o código chega só à janela autenticada, um cliente falso que se registra antes não recebe nada e não aprova, segredo errado, o segredo não está no `cmdline`/`environ` do filho, pedido de outro programa rotulado como tal, pedido trocado vira `consent_superseded`, negar derruba o pedido; o aperto de mão por socketpair (segredo só depois do "pronto"; um daemon que nunca diz "pronto" não recebe segredo e a janela cai no terminal) foi testado com um substituto em bash e com o `kalango-castd` de verdade na sessão aninhada); na sessão aninhada o diálogo apareceu para um pedido feito por `kalango-cast rpc record.start`, mas **ninguém clicou Permitir/Negar nessa sessão** (sem ferramenta de clique) |

Testes que dependem de máquina, rede ou compositor são `#[ignore]` e dizem a variável de ambiente que os liga:
`KALANGO_CAST_GPU` (NVENC), `KALANGO_CAST_OPENH264_NET` (download), `KALANGO_CAST_NIRI` (sessão completa) e `KALANGO_CASTD` (a janela iniciando o `kalango-castd` de verdade como processo filho).
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
- **Consentimento pela janela (feito, com autenticação por segredo)**: a `kalango-cast-app` se registra como **quem
  trata o consentimento** (`consent.handler.register`). Enquanto ela está registrada, o código e a descrição do
  pedido (a origem, o som, o microfone, a pasta e o arquivo, escapados pelo motor) vão **só para ela**, como
  notificações JSON-RPC (`method` "event"); **não são mais impressos no terminal**, e só a conexão registrada aprova
  (`consent.approve`) ou nega (`consent.deny`); outro cliente, mesmo com o código, recebe -32003.
  - **Só quem prova o segredo é tratador.** Quando a janela inicia o `kalango-castd`, os dois ficam com as pontas de
    um **socketpair** (`--secret-fd 3`; não é o ambiente, nem os argumentos, nem um arquivo). O daemon faz
    `prctl(PR_SET_DUMPABLE, 0)` como **a primeira coisa do `main`** (antes de ler argumentos), escreve um byte
    "pronto" no socketpair e **só depois de a janela ler esse byte** ela gera o segredo (256 bits) e o envia; o
    daemon o lê, zera os buffers e fecha o descritor. A janela também faz `PR_SET_DUMPABLE=0` no início, antes de o
    segredo existir na memória dela, e o guarda só na memória. Se o daemon não diz "pronto" em 5 s, nenhum segredo é
    criado nem enviado e a janela segue sem tratador (consentimento no terminal). O registro precisa apresentar o
    segredo (comparação em tempo constante); ausente ou errado: -32003. Nada (código, descrição, silêncio do terminal)
    é entregue antes disso.
  - **Sem segredo, não há tratador.** Um daemon que **não** foi iniciado por uma janela que lhe deu o segredo
    (systemd, ou à mão) não aceita tratador nenhum: o consentimento fica no terminal (ou no diário), com
    `kalango-cast approve CÓDIGO`, como antes. (Um desenho com socketpair para um daemon iniciado pelo systemd fica
    para depois.)
  - **O executável de quem conecta não é credencial** (conectar e depois dar `exec`, ou compartilhar o descritor,
    não se resolve conferindo o pid) e **não é olhado**; não há modo de desenvolvimento que enfraqueça nada.
  - **Risco que resta:** um processo da mesma pessoa que consiga **ptrace/injetar código na janela depois de ela
    ter o segredo, ou ler a memória dela** (`ptrace_scope` frouxo, mesmo usuário, ou root) ainda vence: passa a ser a
    janela. `PR_SET_DUMPABLE=0` fecha o `/proc/<pid>/environ` e a memória a outros processos comuns, mas não a quem
    já injeta na janela por outros meios (`LD_PRELOAD` no ambiente dela, por exemplo, antes de ela abrir). Um
    programa da mesma pessoa também pode tentar se registrar antes da janela, mas sem o segredo é recusado.
- **Cada pedido tem um nome (`request_id`)**: o erro `consent_required` de quem pediu traz o mesmo `request_id` do
  evento enviado à janela; a janela só rotula um pedido como **seu** se o id for o que o `record.start` dela
  recebeu, e mostra os outros como "Pedido de outro programa", com as linhas exatas do motor. Um pedido novo e
  diferente que troca um pedido sem resposta **não é silencioso**: o tratador recebe `consent_superseded` (e o
  terminal, se não há tratador).
- **Indicador**: o daemon avisa início, fim e falha da gravação ao `Hook` (impresso no terminal do `kalango-castd`)
  e à janela registrada. A falha é avisada **no momento em que o escritor a vê** (disco cheio, por exemplo), sem
  esperar um `record.stop`, e a captura de vídeo e de som é parada ali; descartar uma gravação falha no `record.start`
  seguinte não repete o aviso. A **janela tem um indicador sempre visível** (ponto vermelho e cronômetro no alto, fora de
  qualquer rolagem). **Não há indicador do sistema (ilha da barra) nem sinal D-Bus ainda.**
- **`sources.list` e títulos de janela**: o título e o `app_id` das janelas dizem o que a pessoa está fazendo; só
  quem apresenta o segredo do serviço (a janela que o iniciou) os recebe. Qualquer outro programa da mesma pessoa vê
  monitores e ids de janela, com título e `app_id` vazios (a menos que o serviço rode com consentimento `allow`).
  Decisão: não pedir aprovação a cada listagem, porque a janela lista ao abrir e ao atualizar.
- **Aprovação e erro do ambiente**: se o início falha por causa do ambiente (codificador, compositor, o tempo de 10 s),
  a aprovação já dada volta (vale o mesmo prazo), para a pessoa não ser perguntada de novo pelo mesmo pedido.
- **Arquivo de saída** (a pasta precisa ser **sua**: `/tmp`, que é do root, é recusado, com o motivo na resposta;
  use por exemplo `~/Videos`): criado com `O_EXCL|O_NOFOLLOW` (modo 0600) numa pasta que seja sua e não seja um link; só
  termina em `.mp4`, sem `..`; `overwrite` só troca uma gravação nossa anterior (arquivo comum seu, com um só nome,
  vazio ou começando com `ftyp`) — nunca `.bashrc` nem o destino de um link.
- **O que NÃO está feito**: nada impede um programa **hostil da mesma pessoa** que leia o terminal ou a memória do
  daemon (ou edite o `cast.toml` para `allow`); não há diálogo do compositor por janela, nem registro de auditoria,
  nem limite de tempo ocioso nas conexões, nem aviso de que o microfone está aberto fora do daemon; as pastas
  *acima* da pasta de saída podem ser links; o conteúdo gravado não é cifrado.

## A janela (`kalango-cast-app`)

- **O que mostra**: no alto, o indicador (ponto, estado, cronômetro quando grava), o estado do serviço e o botão
  Gravar/Parar; à esquerda, as **cenas** (perfis Gamer, Trabalho e Podcast: só mudam a configuração do motor,
  quadros/s, taxa de bits e quais sons) e as **fontes** (monitores e janelas que o niri oferece); à direita, cartões
  que se arrumam em quantas colunas couberem (prévia, saúde da gravação com quadros/s, perdidos, codificador, taxa
  de bits, tamanho, CPU e GPU, e a saída com a pasta, `~/Vídeos` por padrão, que precisa ser sua). Em janela estreita
  a barra lateral vai para cima dos cartões.
- **A prévia não é uma imagem**: o motor ainda não manda quadros para a janela. A área diz, com o motivo real, se a
  captura está disponível, se faltam fontes ou se o serviço não está lá; durante a gravação diz o tamanho e o
  codificador. Nada é simulado.
- **Serviço**: se o `kalango-castd` não responde no socket da pessoa, a janela o inicia como processo filho (sem
  shell, o socket é o do `$XDG_RUNTIME_DIR`, a saída vai para `castd.log` na mesma pasta) e o encerra ao sair, só se
  ela mesma o iniciou. Fechar a janela durante uma gravação a termina antes.
- **Movimento**: molas simples implementadas no aplicativo (`motion.rs`, sem depender de outro repositório); o relógio
  de quadros só roda enquanto algo se mexe.
- **Textos**: `tr()` do `cast-core` com o catálogo pt_BR (`locales/pt_BR/app.json`); as outras 7 línguas do KalangoOS
  virão depois. Só o tema escuro por enquanto; o claro virá.
- **Ver agora**: `cargo run -p kalango-cast-app` no desktop do computador, ou `scripts/ver-no-niri.sh` para abrir
  na sessão niri aninhada do KalangoOS que já está rodando (descobre o socket Wayland e o barramento D-Bus dela; não
  reinicia nada). `.desktop` e ícone: `crates/kalango-cast-app/dist/` e `assets/logo-kalango-cast.png`.
- **O que não faz**: várias fontes e cenas compostas, mixer, transmissão, gravação ISO, atalhos, Diretor IA; não
  mostra a imagem ao vivo; não escolhe codificador nem codec (H.264 e o melhor disponível); a escolha de fonte
  só tem monitor inteiro ou janela (a área está no serviço, não na janela).

## Usar

```sh
cargo build --release
target/release/kalango-cast sources                         # monitores e janelas
target/release/kalango-cast record --seconds 30 --out gravacao.mp4
target/release/kalango-cast record --window 12 --no-mic --out janela.mp4   # --overwrite troca uma gravação anterior
target/release/kalango-cast record --area DP-1:100,100,1280x720 --codec hevc --out area.mp4
target/release/kalango-cast caps                            # codificadores desta máquina

target/release/kalango-castd &                              # o serviço
target/release/kalango-cast rpc record.start '{"source":{"kind":"monitor","connector":"DP-1"},"path":"'"$HOME"'/Videos/a.mp4"}'   # consent_required: o código aparece no terminal do kalango-castd
target/release/kalango-cast approve CÓDIGO                  # a pessoa aprova; repita o record.start
target/release/kalango-cast rpc record.status
target/release/kalango-cast rpc record.stop
```

Métodos do serviço: `capabilities`, `sources.list`, `record.start` (`source`, `path`, e opcionais `encoder`,
`codec`, `fps`, `bitrate_kbps`, `desktop_audio`, `mic`, `cursor`, `overwrite`; um campo de tipo errado é recusado), `record.status`
(durante um início lento responde `starting: true`, e `record.stop` cancela esse início; fora isso: quadros por segundo,
perdidos, codificador, latência, taxa de bits, CPU, GPU), `record.stop`, e o consentimento: `consent.approve` e
`consent.deny` (`token`) e `consent.handler.register` (para a janela; veja "Privacidade e consentimento").

## Peças

| Crate | O quê |
|---|---|
| `cast-core` | O relógio de mídia, os tipos comuns, as traduções (`tr`) |
| `cast-capture` | ScreenCast do niri por D-Bus, fluxos de vídeo e som do PipeWire |
| `cast-encode` | NVENC (OpenGL ↔ NVENC), sonda da VA-API, OpenH264 (download conferido), Opus |
| `cast-mux` | O gravador de MP4 fragmentado, e um leitor para conferir arquivos |
| `cast-engine` | O motor, o serviço `kalango-castd` e a linha de comando `kalango-cast` |
| `kalango-cast-app` | A janela GTK4 (`kalango-cast-app`): máquina de estados pura (`model.rs`), cliente do socket, ciclo de vida do daemon, molas |
| `cast-testkit` | Sessão niri aninhada, claquete, análise de sincronia, benchmark (só para testes) |

## Desenvolver

Precisa de `libpipewire-0.3-dev`, `libclang-dev`, `libgtk-4-dev` (a janela) e `pkg-config` para compilar; `ffmpeg` para alguns testes. As regras
são as do KalangoOS (veja `AGENTS.md`). O teste de ponta a ponta com um niri de verdade está em
`crates/cast-testkit/tests/niri_session.rs` e o patch do niri que ele usa em `tests/niri/`.

## Licença

GPL-3.0-or-later (`LICENSE`). Avisos de terceiros em `NOTICE`. O logo é marca do KalangoOS.
