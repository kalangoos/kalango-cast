# Desempenho do Kalango Cast: método e resultados medidos

Este documento tem duas partes: **como medir** (o método, que vale mesmo para o que ainda não foi medido) e
**o que foi medido de verdade** (só isso; o resto está marcado como não medido). Nenhum número aqui é projeção.

## 1. O que foi medido

Só o **caminho de codificação do NVENC**, num teste de máquina sem tela, em 2026-11-25:

- GPU: NVIDIA GeForce RTX 3090, driver 580.178.04; contexto OpenGL sem janela (plataforma de dispositivo do EGL).
- Perfil `dev` do workspace (`opt-level = 2`), não `--release`.
- 1920×1080, 300 quadros, 60 quadros/s nominais, bitrate padrão (~12 Mbit/s a 1080p60), IDR forçado no quadro 120.
- Os quadros são **sintéticos e vêm da memória compartilhada** (uma textura enviada da CPU a cada quadro), não DMA-BUF.

| Codec | Quadros/s no total | `encode()` p50 / p99 / máx | CPU do processo | Taxa de bits |
|---|---|---|---|---|
| H.264 | 149,4 | 4,90 / 7,28 / 27,66 ms | 60 % de um núcleo | 14,0 Mbit/s |
| HEVC | 132,3 | 6,00 / 7,47 / 11,32 ms | 49 % de um núcleo | 14,2 Mbit/s |

Como ler estes números (leia antes de citá-los):

- **"Quadros/s no total" não é a velocidade do codificador.** O total divide 300 quadros pelo tempo de parede do laço,
  que inclui o tempo de CPU que o próprio teste gasta preenchendo cada quadro sintético (cerca de 0,45 s dos ~2 s) e o
  envio do quadro da CPU para a GPU. Numa gravação real, os quadros chegam por DMA-BUF e não passam pela CPU.
- **A latência que vale é a de `encode()`** (importar/enviar a textura, desenhar o recorte na GPU, codificar e ler o
  fluxo de bits), medida por chamada, em ms. O máximo (27,66 ms no H.264) é o primeiro quadro e a abertura do contexto
  entrando em regime; com 300 quadros, p99 e máximo são ruído de poucos quadros.
- **A CPU (49–60 %) está inflada**: inclui preencher os quadros sintéticos e enviá-los pelo barramento. Não é o custo
  de CPU de uma gravação; esse número **não foi medido**.
- A saída foi conferida **só por estrutura** (quadros não vazios, o primeiro é IDR, o IDR forçado aparece). Os arquivos
  **não foram decodificados** nem comparados com a origem: qualidade de imagem não foi avaliada.

## 2. Como medir

Faça os testes pelo diretório de alvo persistente (`CARGO_TARGET_DIR`) e em `debug`; use `--release` só para
comparar números. Nada disso roda no CI.

### 2.1 NVENC sem tela (o que foi feito acima)

```sh
KALANGO_CAST_GPU=1 cargo test -p cast-encode --test nvenc -- --ignored --nocapture --test-threads=1
```

Imprime uma linha por codec com quadros/s no total, p50/p99/máximo de `encode()`, CPU do processo (leitura de
`/proc/self/stat`, em % de um núcleo) e taxa de bits. Cuidado ao comparar:

- **quadros/s** mistura o preenchimento dos quadros; use a **latência de `encode()`** para falar do codificador;
- a CPU inclui o teste; para a CPU **do codificador**, meça uma gravação real (2.3);
- rode mais de uma vez e com a GPU parada (`nvidia-smi` sem outro trabalho), ou os números dizem sobre a vizinhança.

### 2.2 Sincronia de áudio e vídeo (claquete)

`crates/cast-testkit` tem a claquete (`kalango-cast-clap`: um clarão e um bipe no mesmo instante) e a análise
(`cast_testkit::analysis`) que mede a diferença entre o quadro do clarão e o bipe no MP4 gravado. Precisa de um niri
aninhado com o patch de `tests/niri/` (`KALANGO_CAST_NIRI`) e de uma tela; faz parte do teste de sessão:

```sh
KALANGO_CAST_NIRI=/caminho/do/niri cargo test -p cast-testkit --test niri_session -- --ignored --nocapture
```

**Não foi executado**: não há resultado de sincronia neste repositório.

### 2.3 Gravação real contra o gravador atual do KalangoOS

`kalango-cast-bench` grava a mesma cena (uma animação em tela cheia, num niri aninhado) com o Kalango Cast e com o
gravador atual do shell (`gst-launch`: pipewiresrc → videoconvert → vp8enc → webmmux) e mede, por gravador: CPU (% de um
núcleo, só do processo gravador), pico de RSS, uso da GPU e do NVENC (`nvidia-smi`, menos a mesma cena sem gravar),
quadros no arquivo contra quadros mostrados, tamanho do arquivo e, no Kalango Cast, a latência da chegada do quadro até
o disco.

```sh
KALANGO_CAST_NIRI=/caminho/do/niri target/release/kalango-cast-bench --cast target/release/kalango-cast \
  --clap target/release/kalango-cast-clap --size 1920x1080 --seconds 20
```

**Não foi executado.** Enquanto não for, não há número de CPU, de latência até o disco, nem de perda de quadros de uma
gravação real, e nenhuma comparação com o gravador atual.

### 2.4 Memória e vazamentos

Meça o RSS ao longo de gravações longas (o `status` do serviço traz `rss_mb`, `dropped`, `audio_dropped`,
`latency_ms_*`). **Não medido.**

## 3. O que NÃO foi medido

- CPU, GPU e latência de uma gravação real (monitor, janela, área), com DMA-BUF.
- Sincronia de áudio e vídeo, correções de deriva, comportamento sob xrun.
- Qualidade do vídeo (nenhuma saída foi decodificada), VA-API e OpenH264 (não rodam).
- Disco lento ou cheio em gravação real (o tratamento do erro tem teste de unidade com um disco que recusa escrita, não
  uma medição).
- Qualquer comparação com o gravador atual do KalangoOS.
