# Kalango Cast — instruções para agentes de código

As regras da casa, as licenças aceitas e os não negociáveis são os do KalangoOS: leia o
[AGENTS.md do KalangoOS](https://github.com/kalangoos/kalangoos/blob/main/AGENTS.md) antes de mexer aqui. O
`CLAUDE.md`, se existir, só aponta para cá.

O que vale a mais neste repositório:

- **Desempenho é requisito.** Meça antes e depois (`kalango-cast-bench`, `docs/DESEMPENHO.md`) e diga os números.
  Quadros não passam pela CPU quando há GPU (DMA-BUF → textura → codificador); nada de alocação por quadro no caminho
  quente (os buffers são reaproveitados).
- **Rust em tudo o que é nosso.** Bibliotecas do sistema (libEGL, libnvidia-encode, libva, libopus, libpipewire) são
  abertas em tempo de execução ou ligadas dinamicamente; `unsafe` só na fronteira FFI, cada bloco com um comentário
  `SAFETY:` dizendo por que vale.
- **Identificadores em inglês; textos para pessoas em pt-BR** por `tr("English")` (`cast_core::i18n`, o mesmo padrão
  do `kalango-i18n` e do Kalango Audio), com a tradução em `crates/cast-core/locales/pt_BR/`. O teste do catálogo falha
  se faltar ou sobrar uma tradução.
- **O OpenH264 da Cisco nunca vai no repositório nem no pacote**: é baixado quando a pessoa pede, e conferido pelo
  SHA-256 fixado em `crates/cast-encode/src/openh264/download.rs`.
- **Testes nunca tocam a pasta pessoal real nem dispositivos reais**: som de teste vai para sinks nulos do PipeWire,
  o microfone de teste é uma fonte virtual, os arquivos ficam em `target/`.

## Construir e testar

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
# a sessão de verdade (niri numa janela, GPU): veja tests/niri/README.md
KALANGO_CAST_NIRI=/caminho/do/niri cargo test -p cast-testkit --test niri_session -- --nocapture
```
